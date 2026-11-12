//! Where a dataset comes from: HuggingFace, Kaggle or GitHub, behind one facade.
//!
//! Models are HuggingFace-only; `Source` only varies for `ModelKind::Dataset`.

use crate::config::{Config, KaggleAuth};
use crate::error::{AppError, AppResult};
use crate::hf::{HfClient, ModelKind, ModelSummary, RepoFile};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const KAGGLE_API: &str = "https://www.kaggle.com/api/v1";
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_RAW: &str = "https://raw.githubusercontent.com";

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    #[default]
    Huggingface,
    Kaggle,
    Github,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Huggingface => "huggingface",
            Source::Kaggle => "kaggle",
            Source::Github => "github",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "huggingface" | "hf" => Some(Source::Huggingface),
            "kaggle" => Some(Source::Kaggle),
            "github" | "gh" => Some(Source::Github),
            _ => None,
        }
    }

    /// Shown when the upstream answers 401/403.
    pub fn auth_hint(self) -> &'static str {
        match self {
            Source::Huggingface => "the repo is gated or private; set HF_TOKEN and restart",
            Source::Kaggle => {
                "private or needs auth; set KAGGLE_USERNAME + KAGGLE_KEY (or KAGGLE_API_TOKEN) \
                 and restart"
            }
            Source::Github => {
                "private or rate limited; set GITHUB_TOKEN and restart (unauthenticated search \
                 is limited to 10 requests/minute)"
            }
        }
    }
}

#[derive(Clone)]
pub struct Hub {
    hf: HfClient,
    kaggle: Option<KaggleAuth>,
    github_token: Option<String>,
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn enc_path(path: &str) -> String {
    path.split('/').map(enc).collect::<Vec<_>>().join("/")
}

fn str_of(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}

impl Hub {
    pub fn new(hf: HfClient, cfg: &Config) -> Self {
        Self { hf, kaggle: cfg.kaggle.clone(), github_token: cfg.github_token.clone() }
    }

    fn kaggle_get(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self.hf.http().get(url);
        match &self.kaggle {
            Some(KaggleAuth::Basic { username, key }) => req.basic_auth(username, Some(key)),
            Some(KaggleAuth::Bearer(token)) => req.bearer_auth(token),
            None => req,
        }
    }

    fn github_get(&self, url: &str) -> reqwest::RequestBuilder {
        let req = self
            .hf
            .http()
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28");
        match &self.github_token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn json(&self, req: reqwest::RequestBuilder, source: Source) -> AppResult<Value> {
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(200).collect();
            return Err(match status.as_u16() {
                404 => AppError::NotFound(format!("not found on {}", source.as_str())),
                401 | 403 => AppError::Upstream(format!("{status} — {}", source.auth_hint())),
                _ => AppError::Upstream(format!("{} {status}: {snippet}", source.as_str())),
            });
        }
        Ok(resp.json::<Value>().await?)
    }

    pub async fn search(
        &self,
        source: Source,
        query: &str,
        limit: usize,
    ) -> AppResult<Vec<ModelSummary>> {
        let limit = limit.clamp(1, 100);
        match source {
            Source::Huggingface => self.hf.search(query, ModelKind::Dataset, limit).await,
            Source::Kaggle => {
                let url = format!(
                    "{KAGGLE_API}/datasets/list?search={}&sortBy=hottest&page=1",
                    enc(query.trim())
                );
                let v = self.json(self.kaggle_get(&url), source).await?;
                Ok(v.as_array()
                    .map(|a| a.iter().take(limit).filter_map(kaggle_summary).collect())
                    .unwrap_or_default())
            }
            Source::Github => {
                // The search API rejects an empty query.
                let q = match query.trim() {
                    "" => "topic:dataset".to_string(),
                    q => q.to_string(),
                };
                let url = format!(
                    "{GITHUB_API}/search/repositories?q={}&sort=stars&order=desc&per_page={limit}",
                    enc(&q)
                );
                let v = self.json(self.github_get(&url), source).await?;
                Ok(v.get("items")
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(github_summary).collect())
                    .unwrap_or_default())
            }
        }
    }

    /// Repo summary plus the revision to use: the caller's, or the source's own default
    /// (`main`, the Kaggle current version, the GitHub default branch).
    pub async fn info(
        &self,
        source: Source,
        repo_id: &str,
        revision: Option<&str>,
    ) -> AppResult<(ModelSummary, String)> {
        match source {
            Source::Huggingface => {
                let rev = revision.unwrap_or("main").to_string();
                let info = self.hf.model_info(repo_id, &rev, ModelKind::Dataset).await?;
                Ok((info, rev))
            }
            Source::Kaggle => {
                let v = self
                    .json(self.kaggle_get(&format!("{KAGGLE_API}/datasets/view/{repo_id}")), source)
                    .await?;
                let mut summary = kaggle_summary(&v).unwrap_or_else(|| empty_summary(repo_id));
                summary.id = repo_id.to_string();
                let rev = revision
                    .map(str::to_string)
                    .or_else(|| {
                        v.get("currentVersionNumber").and_then(Value::as_u64).map(|n| n.to_string())
                    })
                    .unwrap_or_else(|| "latest".into());
                Ok((summary, rev))
            }
            Source::Github => {
                let v = self
                    .json(self.github_get(&format!("{GITHUB_API}/repos/{repo_id}")), source)
                    .await?;
                let mut summary = github_summary(&v).unwrap_or_else(|| empty_summary(repo_id));
                summary.id = repo_id.to_string();
                let rev = revision
                    .map(str::to_string)
                    .or_else(|| str_of(&v, "default_branch"))
                    .unwrap_or_else(|| "HEAD".into());
                Ok((summary, rev))
            }
        }
    }

    pub async fn list_files(
        &self,
        source: Source,
        repo_id: &str,
        revision: &str,
    ) -> AppResult<Vec<RepoFile>> {
        let mut files = match source {
            Source::Huggingface => {
                return self.hf.list_files(repo_id, revision, ModelKind::Dataset).await;
            }
            Source::Kaggle => {
                let mut files = Vec::new();
                let mut page_token = String::new();
                for _ in 0..200 {
                    let mut url = format!("{KAGGLE_API}/datasets/list/{repo_id}?pageSize=200");
                    if let Ok(n) = revision.parse::<u64>() {
                        url.push_str(&format!("&datasetVersionNumber={n}"));
                    }
                    if !page_token.is_empty() {
                        url.push_str(&format!("&pageToken={}", enc(&page_token)));
                    }
                    let v = self.json(self.kaggle_get(&url), source).await?;
                    for f in v.get("datasetFiles").and_then(Value::as_array).into_iter().flatten() {
                        if let Some(name) = str_of(f, "name") {
                            files.push(RepoFile {
                                relevant: true,
                                size: u64_of(f, "totalBytes"),
                                path: name,
                            });
                        }
                    }
                    page_token = str_of(&v, "nextPageToken").unwrap_or_default();
                    if page_token.is_empty() {
                        break;
                    }
                }
                files
            }
            Source::Github => {
                let url =
                    format!("{GITHUB_API}/repos/{repo_id}/git/trees/{}?recursive=1", enc(revision));
                let v = self.json(self.github_get(&url), source).await?;
                if v.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
                    return Err(AppError::Upstream(
                        "this repo has too many files for GitHub to list; clone it instead".into(),
                    ));
                }
                v.get("tree")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter(|e| e.get("type").and_then(Value::as_str) == Some("blob"))
                    .filter_map(|e| {
                        let path = str_of(e, "path")?;
                        let name = path.rsplit('/').next().unwrap_or(&path);
                        Some(RepoFile {
                            relevant: name != ".gitattributes",
                            size: u64_of(e, "size"),
                            path,
                        })
                    })
                    .collect()
            }
        };
        files.sort_by(|a, b| b.relevant.cmp(&a.relevant).then_with(|| a.path.cmp(&b.path)));
        Ok(files)
    }

    /// The authenticated GET for one file. Redirects (Kaggle → GCS) are followed by reqwest,
    /// which drops our credentials when they cross hosts.
    pub fn file_request(
        &self,
        source: Source,
        kind: ModelKind,
        repo_id: &str,
        revision: &str,
        path: &str,
    ) -> reqwest::RequestBuilder {
        match source {
            Source::Huggingface => {
                let url = self.hf.resolve_url(kind, repo_id, revision, path);
                self.hf.authorize(self.hf.http().get(url))
            }
            Source::Kaggle => {
                let mut url =
                    format!("{KAGGLE_API}/datasets/download/{repo_id}/{}", enc_path(path));
                if let Ok(n) = revision.parse::<u64>() {
                    url.push_str(&format!("?datasetVersionNumber={n}"));
                }
                self.kaggle_get(&url)
            }
            Source::Github => {
                let url =
                    format!("{GITHUB_RAW}/{repo_id}/{}/{}", enc_path(revision), enc_path(path));
                let req = self.hf.http().get(url);
                match &self.github_token {
                    Some(t) => req.bearer_auth(t),
                    None => req,
                }
            }
        }
    }
}

fn empty_summary(id: &str) -> ModelSummary {
    ModelSummary {
        id: id.to_string(),
        downloads: 0,
        likes: 0,
        pipeline_tag: None,
        last_modified: None,
        gated: false,
        tags: Vec::new(),
        description: None,
    }
}

fn kaggle_summary(v: &Value) -> Option<ModelSummary> {
    let id = str_of(v, "ref")?;
    let description = match (str_of(v, "title"), str_of(v, "subtitle")) {
        (Some(t), Some(s)) => Some(format!("{t} — {s}")),
        (t, s) => t.or(s),
    };
    Some(ModelSummary {
        id,
        downloads: u64_of(v, "downloadCount"),
        likes: u64_of(v, "voteCount"),
        pipeline_tag: None,
        last_modified: str_of(v, "lastUpdated"),
        gated: false,
        tags: str_of(v, "licenseName").into_iter().collect(),
        description,
    })
}

fn github_summary(v: &Value) -> Option<ModelSummary> {
    Some(ModelSummary {
        id: str_of(v, "full_name")?,
        downloads: 0,
        likes: u64_of(v, "stargazers_count"),
        pipeline_tag: None,
        last_modified: str_of(v, "pushed_at"),
        gated: v.get("private").and_then(Value::as_bool).unwrap_or(false),
        tags: v
            .get("topics")
            .and_then(Value::as_array)
            .map(|t| t.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        description: str_of(v, "description"),
    })
}

/// What a pasted repo reference said: `owner/name`, or a Hub / Kaggle / GitHub URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub repo_id: String,
    /// Known when the input was a URL (or `hf://`); `None` for a bare `owner/name`.
    pub source: Option<Source>,
    /// `Some(true)` for a `/datasets/` URL, `Some(false)` for a plain HF model URL.
    pub dataset: Option<bool>,
    pub revision: Option<String>,
}

pub fn parse_repo_ref(input: &str) -> AppResult<RepoRef> {
    let bad = || AppError::BadRequest(format!("can't make a repo out of {input:?}"));
    let mut s = input.trim();
    s = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    let mut source = None;
    let mut dataset = None;

    let (host, rest) = if let Some(rest) = s.strip_prefix("hf://") {
        ("huggingface.co", rest)
    } else if let Some(rest) = s.strip_prefix("git@github.com:") {
        ("github.com", rest)
    } else {
        let no_scheme = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://"));
        match no_scheme {
            Some(url) => {
                let url = url.strip_prefix("www.").unwrap_or(url);
                url.split_once('/').ok_or_else(bad)?
            }
            None => ("", s),
        }
    };
    let mut segs: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();

    match host {
        "" => {}
        "huggingface.co" | "hf.co" => {
            source = Some(Source::Huggingface);
            dataset = Some(false);
            if segs.first() == Some(&"datasets") {
                dataset = Some(true);
                segs.remove(0);
            } else if segs.first() == Some(&"spaces") {
                return Err(AppError::BadRequest("Spaces can't be downloaded".into()));
            }
        }
        "kaggle.com" => {
            source = Some(Source::Kaggle);
            dataset = Some(true);
            if segs.first() != Some(&"datasets") {
                return Err(AppError::BadRequest(
                    "only kaggle.com/datasets/... is supported".into(),
                ));
            }
            segs.remove(0);
        }
        "github.com" => {
            source = Some(Source::Github);
            dataset = Some(true);
        }
        other => return Err(AppError::BadRequest(format!("unsupported host {other:?}"))),
    }
    if host.is_empty() {
        if let Some(r) = segs.first().and_then(|f| f.strip_prefix("datasets")) {
            // "datasets/owner/name" shorthand
            if r.is_empty() && segs.len() == 3 {
                dataset = Some(true);
                segs.remove(0);
            }
        }
    }

    if segs.len() < 2 && !(segs.len() == 1 && source.is_none()) {
        return Err(bad());
    }
    // `.../tree/<rev>`, `.../blob/<rev>/...`, `.../resolve/<rev>/...` carry a revision.
    let revision = match segs.get(2) {
        Some(&("tree" | "blob" | "resolve")) => segs.get(3).map(|r| r.to_string()),
        _ => None,
    };
    let owner_name: Vec<&str> = if segs.len() >= 2 { segs[..2].to_vec() } else { segs.clone() };
    let mut repo_id = owner_name.join("/");
    if source == Some(Source::Github) {
        repo_id = repo_id.strip_suffix(".git").unwrap_or(&repo_id).to_string();
    }
    Ok(RepoRef { repo_id, source, dataset, revision })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoRef {
        parse_repo_ref(s).unwrap()
    }

    #[test]
    fn bare_ids() {
        let r = p("bvidgen/Dynamically-Generated-Hate-Speech-Dataset");
        assert_eq!(r.repo_id, "bvidgen/Dynamically-Generated-Hate-Speech-Dataset");
        assert_eq!((r.source, r.dataset, r.revision), (None, None, None));
        assert_eq!(p("  org/name/ ").repo_id, "org/name");
        assert_eq!(p("datasets/org/name").dataset, Some(true));
        assert_eq!(p("datasets/org/name").repo_id, "org/name");
    }

    #[test]
    fn urls() {
        let r = p("https://huggingface.co/datasets/bvidgen/Dyn-Data/tree/main");
        assert_eq!(r.repo_id, "bvidgen/Dyn-Data");
        assert_eq!((r.source, r.dataset), (Some(Source::Huggingface), Some(true)));
        assert_eq!(r.revision.as_deref(), Some("main"));

        let r = p("https://huggingface.co/Qwen/Qwen3-8B");
        assert_eq!((r.repo_id.as_str(), r.dataset), ("Qwen/Qwen3-8B", Some(false)));
        assert_eq!(p("hf://datasets/a/b").repo_id, "a/b");

        let r = p("https://www.kaggle.com/datasets/heptapod/titanic?select=x.csv");
        assert_eq!((r.repo_id.as_str(), r.source), ("heptapod/titanic", Some(Source::Kaggle)));

        let r = p("https://github.com/mwaskom/seaborn-data.git");
        assert_eq!((r.repo_id.as_str(), r.source), ("mwaskom/seaborn-data", Some(Source::Github)));
        assert_eq!(p("git@github.com:a/b.git").repo_id, "a/b");
        assert_eq!(p("https://github.com/a/b/tree/dev").revision.as_deref(), Some("dev"));
    }

    #[test]
    fn rejects() {
        assert!(parse_repo_ref("https://example.com/a/b").is_err());
        assert!(parse_repo_ref("https://huggingface.co/spaces/a/b").is_err());
        assert!(parse_repo_ref("https://kaggle.com/competitions/x").is_err());
        assert!(parse_repo_ref("https://github.com/onlyowner").is_err());
    }
}
