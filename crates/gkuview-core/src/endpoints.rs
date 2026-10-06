use serde::{Deserialize, Serialize};

use crate::url_util;

pub fn default_file_base() -> String {
    "/DCIM/100MEDIA".to_string()
}

fn default_list_url_template() -> Option<String> {
    Some("/app/getfilelist?folder={folder}&start={start}&end={end}".to_string())
}

fn default_folders() -> Vec<String> {
    vec![
        "loop".to_string(),
        "event".to_string(),
        "emr".to_string(),
        "park".to_string(),
        "race".to_string(),
    ]
}

fn default_page_size() -> u32 {
    100
}

fn default_device_info_paths() -> Vec<String> {
    vec![
        "/app/getproductinfo".to_string(),
        "/app/getdeviceattr".to_string(),
    ]
}

fn default_media_info_path() -> Option<String> {
    Some("/app/getmediainfo".to_string())
}

fn default_file_url_template() -> String {
    "{base}/{path}".to_string()
}

fn default_rtsp_template() -> String {
    "rtsp://{host}:554/livestream/12".to_string()
}

fn default_thumbnail_template() -> Option<String> {
    Some("{base}/app/getthumbnail?file={path}".to_string())
}

fn default_size_scale() -> u64 {
    1024
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListCandidate {
    pub label: String,
    pub path: String,
}

impl ListCandidate {
    pub fn new(label: &str, path: &str) -> Self {
        Self {
            label: label.to_string(),
            path: path.to_string(),
        }
    }
}

/// Endpoint configuration for the dashcam HTTP API.
///
/// Defaults are the endpoints verified on a GKU D600Pro (2026-10-06 payload
/// capture, see `docs/dashcam-api.md`). The candidate list is kept for other
/// firmwares; it is only probed when the template listing yields nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoints {
    /// Paginated listing template. Placeholders: `{folder}`, `{start}`, `{end}`.
    /// Set to `null` to disable and rely on `list_candidates` only.
    #[serde(default = "default_list_url_template")]
    pub list_url_template: Option<String>,
    #[serde(default = "default_folders")]
    pub folders: Vec<String>,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// Paths probed for model/firmware information.
    #[serde(default = "default_device_info_paths")]
    pub device_info_paths: Vec<String>,
    /// Path returning live RTSP URLs (JSON).
    #[serde(default = "default_media_info_path")]
    pub media_info_path: Option<String>,
    #[serde(default)]
    pub list_candidates: Vec<ListCandidate>,
    /// Directory used to resolve relative paths returned by command-style listings.
    #[serde(default = "default_file_base")]
    pub file_base: String,
    /// Template for fetching a media file. Placeholders: `{base}`, `{host}`, `{path}`, `{name}`.
    #[serde(default = "default_file_url_template")]
    pub file_url_template: String,
    /// RTSP URL template for the live view. Placeholders: `{host}`.
    #[serde(default = "default_rtsp_template")]
    pub rtsp_url_template: String,
    /// Optional thumbnail template. Placeholders: `{base}`, `{host}`, `{path}`, `{name}`.
    #[serde(default = "default_thumbnail_template")]
    pub thumbnail_url_template: Option<String>,
    /// Listing `size` fields are in KiB on GKU/Shenshu firmwares.
    #[serde(default = "default_size_scale")]
    pub list_size_scale: u64,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            list_url_template: default_list_url_template(),
            folders: default_folders(),
            page_size: default_page_size(),
            device_info_paths: default_device_info_paths(),
            media_info_path: default_media_info_path(),
            list_candidates: vec![
                ListCandidate::new(
                    "GKU/Shenshu app API, loop folder",
                    "/app/getfilelist?folder=loop&start=0&end=99",
                ),
                ListCandidate::new(
                    "Novatek-style command API (unverified)",
                    "/?custom=1&cmd=3011&par=0",
                ),
                ListCandidate::new("Directory index /DCIM/100MEDIA/", "/DCIM/100MEDIA/"),
            ],
            file_base: default_file_base(),
            file_url_template: default_file_url_template(),
            rtsp_url_template: default_rtsp_template(),
            thumbnail_url_template: default_thumbnail_template(),
            list_size_scale: default_size_scale(),
        }
    }
}

impl Endpoints {
    pub fn expand(template: &str, base: &str, host: &str, path: &str) -> String {
        let name = url_util::basename(path);
        let out = template
            .replace("{base}", base)
            .replace("{host}", host)
            .replace("{path}", path)
            .replace("{name}", name);
        url_util::encode_path(&out)
    }

    pub fn file_url(&self, base: &str, host: &str, path: &str) -> String {
        let expanded = self
            .file_url_template
            .replace("{base}", base)
            .replace("{host}", host)
            .replace("{path}", path)
            .replace("{name}", url_util::basename(path));
        let (head, query) = match expanded.split_once('?') {
            Some((h, q)) => (h.to_string(), Some(q.to_string())),
            None => (expanded, None),
        };
        let encoded = url_util::encode_path(&head);
        match query {
            Some(q) => format!("{encoded}?{q}"),
            None => encoded,
        }
    }

    pub fn rtsp_url(&self, host: &str) -> String {
        self.rtsp_url_template.replace("{host}", host)
    }

    pub fn thumbnail_url(&self, base: &str, host: &str, path: &str) -> Option<String> {
        self.thumbnail_url_template
            .as_ref()
            .map(|t| Self::expand(t, base, host, path))
    }

    pub fn list_page_url(&self, folder: &str, start: u32, end: u32) -> Option<String> {
        self.list_url_template.as_ref().map(|t| {
            t.replace("{folder}", folder)
                .replace("{start}", &start.to_string())
                .replace("{end}", &end.to_string())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_expand() {
        let e = Endpoints::default();
        assert_eq!(
            e.file_url("http://192.168.0.1", "192.168.0.1", "sd//Video/A.TS"),
            "http://192.168.0.1/sd//Video/A.TS"
        );
        assert_eq!(
            e.rtsp_url("192.168.0.1"),
            "rtsp://192.168.0.1:554/livestream/12"
        );
        assert_eq!(
            e.thumbnail_url("http://192.168.0.1", "192.168.0.1", "sd//Video/A.TS"),
            Some("http://192.168.0.1/app/getthumbnail?file=sd//Video/A.TS".to_string())
        );
    }

    #[test]
    fn spaces_are_encoded() {
        let e = Endpoints::default();
        assert_eq!(
            e.file_url("http://cam", "cam", "DCIM/100MEDIA/IMG 1.JPG"),
            "http://cam/DCIM/100MEDIA/IMG%201.JPG"
        );
    }

    #[test]
    fn list_page_url_builds() {
        let e = Endpoints::default();
        assert_eq!(
            e.list_page_url("loop", 100, 199).unwrap(),
            "/app/getfilelist?folder=loop&start=100&end=199"
        );
    }
}
