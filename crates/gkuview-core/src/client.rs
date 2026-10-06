use std::io::{Read, Write};
use std::sync::Mutex;
use std::time::Duration;

use crate::endpoints::{Endpoints, ListCandidate};
use crate::error::{Error, Result};
use crate::listing;
use crate::model::{Clip, DeviceInfo, RangeInfo};
use crate::url_util;

pub const USER_AGENT: &str = concat!("GKUview/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Default)]
pub struct Fetched {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
    pub range: RangeInfo,
}

#[derive(Debug, Clone)]
pub struct EndpointProbe {
    pub label: String,
    pub path: String,
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub clips: usize,
    pub sample: String,
    pub error: Option<String>,
}

pub struct DashcamClient {
    host: String,
    endpoints: Endpoints,
    agent: ureq::Agent,
    resolved_list: Mutex<Option<ListCandidate>>,
}

fn normalize_host(input: &str) -> String {
    let host = input
        .trim()
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    if host.is_empty() {
        crate::config::DEFAULT_HOST.to_string()
    } else {
        host.to_string()
    }
}

fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim().parse().ok()?;
    let total = if total.trim() == "*" {
        None
    } else {
        total.trim().parse().ok()
    };
    Some((start, end, total))
}

fn range_info(
    status: u16,
    headers: &ureq::http::HeaderMap,
    content_length: Option<u64>,
) -> RangeInfo {
    let accept_ranges = headers
        .get("accept-ranges")
        .and_then(|v| v.to_str().ok())
        .map(|v| !v.eq_ignore_ascii_case("none"))
        .unwrap_or(false);
    let content_range = headers
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range);
    RangeInfo {
        status,
        content_length,
        accept_ranges,
        content_range,
    }
}

fn copy_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    done: &mut u64,
    total: u64,
    progress: &mut dyn FnMut(u64, u64),
) -> std::io::Result<()> {
    let mut buf = [0u8; 128 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        writer.write_all(&buf[..n])?;
        *done += n as u64;
        progress(*done, total);
    }
    Ok(())
}

impl DashcamClient {
    pub fn new(host: impl Into<String>, endpoints: Endpoints) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
            .build();
        Self {
            host: normalize_host(&host.into()),
            endpoints,
            agent: config.into(),
            resolved_list: Mutex::new(None),
        }
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.host)
    }

    pub fn url(&self, path: &str) -> String {
        url_util::join_url(&self.base_url(), path)
    }

    pub fn file_url(&self, path: &str) -> String {
        self.endpoints.file_url(&self.base_url(), &self.host, path)
    }

    pub fn rtsp_url(&self) -> String {
        self.endpoints.rtsp_url(&self.host)
    }

    pub fn get(&self, url: &str) -> Result<Fetched> {
        self.get_limited(url, usize::MAX)
    }

    pub fn get_limited(&self, url: &str, limit: usize) -> Result<Fetched> {
        let res = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| map_ureq_error(e, url))?;
        self.collect(res, url, limit)
    }

    fn collect(
        &self,
        mut res: ureq::http::Response<ureq::Body>,
        url: &str,
        limit: usize,
    ) -> Result<Fetched> {
        let status = res.status().as_u16();
        let content_type = res
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let content_length = res.body().content_length();
        let range = range_info(status, res.headers(), content_length);
        let mut body = Vec::new();
        if limit == usize::MAX {
            res.body_mut().as_reader().read_to_end(&mut body)?;
        } else {
            res.body_mut()
                .as_reader()
                .take(limit as u64)
                .read_to_end(&mut body)?;
        }
        let _ = url;
        Ok(Fetched {
            status,
            content_type,
            body,
            range,
        })
    }

    pub fn get_range(&self, url: &str, start: u64, end: u64) -> Result<(Vec<u8>, RangeInfo)> {
        let mut res = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Range", format!("bytes={start}-{end}"))
            .call()
            .map_err(|e| map_ureq_error(e, url))?;
        let status = res.status().as_u16();
        if status == 200 {
            return Err(Error::RangeUnsupported(200));
        }
        if status != 206 {
            return Err(Error::HttpStatus {
                status,
                url: url.to_string(),
            });
        }
        let content_length = res.body().content_length();
        let info = range_info(status, res.headers(), content_length);
        let mut body = Vec::with_capacity((end.saturating_sub(start) + 1) as usize);
        res.body_mut().as_reader().read_to_end(&mut body)?;
        Ok((body, info))
    }

    pub fn head(&self, url: &str) -> Result<RangeInfo> {
        if let Ok(res) = self.agent.head(url).header("User-Agent", USER_AGENT).call() {
            let status = res.status().as_u16();
            if (200..300).contains(&status) {
                let content_length = res.body().content_length();
                if content_length.is_some() {
                    return Ok(range_info(status, res.headers(), content_length));
                }
            }
        }
        let probe = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Range", "bytes=0-0")
            .call()
            .map_err(|e| map_ureq_error(e, url))?;
        let probe_status = probe.status().as_u16();
        if probe_status >= 400 {
            return Err(Error::HttpStatus {
                status: probe_status,
                url: url.to_string(),
            });
        }
        let content_length = probe.body().content_length();
        Ok(range_info(probe_status, probe.headers(), content_length))
    }

    pub fn fetch_range_to<W: Write>(
        &self,
        url: &str,
        start: u64,
        end: u64,
        writer: &mut W,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<RangeInfo> {
        let res = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Range", format!("bytes={start}-{end}"))
            .call()
            .map_err(|e| map_ureq_error(e, url))?;
        let status = res.status().as_u16();
        let content_length = res.body().content_length();
        let info = range_info(status, res.headers(), content_length);
        if status == 200 {
            return Err(Error::RangeUnsupported(200));
        }
        if status != 206 {
            return Err(Error::HttpStatus {
                status,
                url: url.to_string(),
            });
        }
        let expected = end.saturating_sub(start) + 1;
        let total = expected;
        let mut res = res;
        let mut reader = res.body_mut().as_reader().take(expected);
        let mut done = 0u64;
        copy_with_progress(&mut reader, writer, &mut done, total, progress)?;
        Ok(info)
    }

    pub fn download_to<W: Write>(
        &self,
        url: &str,
        writer: &mut W,
        progress: &mut dyn FnMut(u64, u64),
    ) -> Result<u64> {
        let mut res = self
            .agent
            .get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| map_ureq_error(e, url))?;
        let status = res.status().as_u16();
        if status >= 400 {
            return Err(Error::HttpStatus {
                status,
                url: url.to_string(),
            });
        }
        let total = res.body().content_length().unwrap_or(0);
        let mut reader = res.body_mut().as_reader();
        let mut done = 0u64;
        copy_with_progress(&mut reader, writer, &mut done, total, progress)?;
        Ok(done)
    }

    pub fn device_info(&self) -> Result<DeviceInfo> {
        let mut model: Option<String> = None;
        let mut firmware: Option<String> = None;
        let mut raw_parts: Vec<String> = Vec::new();
        let mut last_error = None;

        for path in &self.endpoints.device_info_paths {
            let url = self.url(path);
            match self.get_limited(&url, 32 * 1024) {
                Ok(fetched) if fetched.status < 400 => {
                    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&fetched.body) {
                        if let Some(info) = value.get("info") {
                            if model.is_none() {
                                model = json_str(info, &["model", "device", "product"]);
                            }
                            if firmware.is_none() {
                                firmware = json_str(info, &["softver", "firmware", "version"]);
                            }
                        }
                    }
                    let snippet: String = String::from_utf8_lossy(&fetched.body)
                        .chars()
                        .take(256)
                        .collect();
                    raw_parts.push(snippet);
                }
                Ok(fetched) => {
                    last_error = Some(Error::HttpStatus {
                        status: fetched.status,
                        url,
                    })
                }
                Err(e) => last_error = Some(e),
            }
        }

        if model.is_none() && firmware.is_none() {
            for path in ["/", "/?custom=1&cmd=1001"] {
                let url = self.url(path);
                if let Ok(fetched) = self.get_limited(&url, 16 * 1024) {
                    if fetched.status < 400 {
                        let text = String::from_utf8_lossy(&fetched.body).to_string();
                        model = extract_value(&text, &["model", "device", "product"]);
                        firmware = extract_value(&text, &["firmware", "version", "fw", "sw"]);
                        raw_parts.push(text.chars().take(256).collect());
                        break;
                    }
                }
            }
        }

        if model.is_none() && firmware.is_none() && raw_parts.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| Error::Protocol("no device info endpoint responded".into())));
        }

        Ok(DeviceInfo {
            host: self.host.clone(),
            model,
            firmware,
            raw: Some(raw_parts.join(" | ")),
        })
    }

    /// Live RTSP URLs reported by the device, front camera first.
    pub fn rtsp_urls(&self) -> Result<Vec<String>> {
        let fallback = vec![self.rtsp_url()];
        let Some(path) = self.endpoints.media_info_path.as_deref() else {
            return Ok(fallback);
        };
        let url = self.url(path);
        let fetched = self.get_limited(&url, 64 * 1024)?;
        if fetched.status >= 400 {
            return Err(Error::HttpStatus {
                status: fetched.status,
                url,
            });
        }
        let value: serde_json::Value = serde_json::from_slice(&fetched.body)?;
        let urls: Vec<String> = value
            .pointer("/info/rtsps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            Ok(fallback)
        } else {
            Ok(urls)
        }
    }

    /// Fetches the device-rendered JPEG thumbnail, when the firmware supports it.
    pub fn fetch_thumbnail(&self, clip: &Clip) -> Result<Option<Vec<u8>>> {
        let Some(url) = self
            .endpoints
            .thumbnail_url(&self.base_url(), &self.host, &clip.path)
        else {
            return Ok(None);
        };
        let fetched = self.get_limited(&url, 4 * 1024 * 1024)?;
        if fetched.status >= 400 || fetched.body.is_empty() {
            return Ok(None);
        }
        Ok(Some(fetched.body))
    }

    pub fn list_clips(&self) -> Result<Vec<Clip>> {
        if let Some(template) = self.endpoints.list_url_template.clone() {
            match self.list_clips_paginated(&template) {
                Ok(clips) if !clips.is_empty() => return Ok(clips),
                Ok(_) => {}
                Err(_) => {}
            }
        }

        let cached = self.resolved_list.lock().unwrap().clone();
        if let Some(candidate) = cached {
            if let Ok(clips) = self.fetch_list(&candidate) {
                if !clips.is_empty() {
                    return Ok(clips);
                }
            }
            *self.resolved_list.lock().unwrap() = None;
        }

        let mut best: Option<(Vec<Clip>, ListCandidate)> = None;
        let mut errors = Vec::new();
        for candidate in &self.endpoints.list_candidates {
            match self.fetch_list(candidate) {
                Ok(clips) if !clips.is_empty() => {
                    let better = best
                        .as_ref()
                        .map(|(current, _)| clips.len() > current.len())
                        .unwrap_or(true);
                    if better {
                        best = Some((clips, candidate.clone()));
                    }
                }
                Ok(_) => {}
                Err(e) => errors.push(format!("{}: {e}", candidate.label)),
            }
        }

        match best {
            Some((clips, candidate)) => {
                *self.resolved_list.lock().unwrap() = Some(candidate);
                Ok(clips)
            }
            None => Err(Error::Protocol(format!(
                "no listing endpoint returned clips. Tried: {}. \
                 Capture the app's requests (see docs/capture-analysis.md) and update \
                 the endpoints in your config file.",
                if errors.is_empty() {
                    "only empty responses".to_string()
                } else {
                    errors.join("; ")
                }
            ))),
        }
    }

    fn list_clips_paginated(&self, template: &str) -> Result<Vec<Clip>> {
        let mut all: Vec<Clip> = Vec::new();
        let mut last_error: Option<Error> = None;
        let mut any_response = false;
        let page_size = self.endpoints.page_size.max(1);

        for folder in &self.endpoints.folders {
            let mut start: u32 = 0;
            for _page in 0..200 {
                let Some(path) = self
                    .endpoints
                    .list_page_url(folder, start, start + page_size - 1)
                else {
                    break;
                };
                let url = self.url(&path);
                let fetched = match self.get_limited(&url, 8 * 1024 * 1024) {
                    Ok(fetched) => fetched,
                    Err(e) => {
                        last_error = Some(e);
                        break;
                    }
                };
                any_response = true;
                if fetched.status >= 400 {
                    last_error = Some(Error::HttpStatus {
                        status: fetched.status,
                        url,
                    });
                    break;
                }
                if parse_result_code(&fetched.body)
                    .map(|c| c < 0)
                    .unwrap_or(false)
                {
                    break;
                }
                let clips = listing::parse_clip_list(
                    &fetched.body,
                    template,
                    &self.endpoints.file_base,
                    self.endpoints.list_size_scale,
                );
                let count = clips.len();
                all.extend(clips);
                if count < page_size as usize {
                    break;
                }
                start += page_size;
            }
        }

        if all.is_empty() {
            if let Some(e) = last_error {
                return Err(e);
            }
            if !any_response {
                return Err(Error::Protocol(
                    "listing template returned no responses".into(),
                ));
            }
        }
        listing::sort_clips(&mut all);
        all.dedup_by(|a, b| a.path == b.path);
        Ok(all)
    }

    fn fetch_list(&self, candidate: &ListCandidate) -> Result<Vec<Clip>> {
        let url = self.url(&candidate.path);
        let fetched = self.get_limited(&url, 4 * 1024 * 1024)?;
        if fetched.status >= 400 {
            return Err(Error::HttpStatus {
                status: fetched.status,
                url,
            });
        }
        Ok(listing::parse_clip_list(
            &fetched.body,
            &candidate.path,
            &self.endpoints.file_base,
            self.endpoints.list_size_scale,
        ))
    }

    pub fn probe(&self) -> Vec<EndpointProbe> {
        self.endpoints
            .list_candidates
            .iter()
            .map(|candidate| {
                let url = self.url(&candidate.path);
                match self.get_limited(&url, 64 * 1024) {
                    Ok(fetched) => {
                        let clips = listing::parse_clip_list(
                            &fetched.body,
                            &candidate.path,
                            &self.endpoints.file_base,
                            self.endpoints.list_size_scale,
                        );
                        let sample: String = String::from_utf8_lossy(&fetched.body)
                            .chars()
                            .filter(|c| !c.is_control() || *c == '\n')
                            .take(240)
                            .collect();
                        EndpointProbe {
                            label: candidate.label.clone(),
                            path: candidate.path.clone(),
                            url,
                            status: fetched.status,
                            content_type: fetched.content_type,
                            clips: clips.len(),
                            sample,
                            error: None,
                        }
                    }
                    Err(e) => EndpointProbe {
                        label: candidate.label.clone(),
                        path: candidate.path.clone(),
                        url,
                        status: 0,
                        content_type: String::new(),
                        clips: 0,
                        sample: String::new(),
                        error: Some(e.to_string()),
                    },
                }
            })
            .collect()
    }
}

fn json_str(value: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
}

fn parse_result_code(body: &[u8]) -> Option<i64> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("result").and_then(|r| r.as_i64()))
}

fn map_ureq_error(e: ureq::Error, url: &str) -> Error {
    match e {
        ureq::Error::StatusCode(code) => Error::HttpStatus {
            status: code,
            url: url.to_string(),
        },
        other => Error::Network(other.to_string()),
    }
}

fn extract_value(text: &str, keys: &[&str]) -> Option<String> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        for key in keys {
            if let Some(pos) = lower.find(key) {
                let after = &line[pos + key.len()..];
                let after = after.trim_start_matches([':', '=', ' ', '"', '\'']);
                let value: String = after
                    .chars()
                    .take_while(|c| !matches!(c, '"' | '\'' | ',' | '}' | '<' | '\r' | '\n'))
                    .collect();
                let value = value.trim().to_string();
                if !value.is_empty() {
                    return Some(value);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-1023/5000"),
            Some((0, 1023, Some(5000)))
        );
        assert_eq!(
            parse_content_range("bytes 100-199/*"),
            Some((100, 199, None))
        );
        assert_eq!(parse_content_range("garbage"), None);
    }

    #[test]
    fn host_normalisation() {
        assert_eq!(normalize_host("http://192.168.0.1/"), "192.168.0.1");
        assert_eq!(normalize_host("192.168.0.1:8080"), "192.168.0.1:8080");
        assert_eq!(normalize_host(""), "192.168.0.1");
    }

    #[test]
    fn kv_extraction() {
        assert_eq!(
            extract_value("model: GKU-D900\nfw=1.2.3", &["model"]),
            Some("GKU-D900".to_string())
        );
        assert_eq!(
            extract_value("model: GKU-D900\nfw=1.2.3", &["fw", "firmware"]),
            Some("1.2.3".to_string())
        );
    }
}
