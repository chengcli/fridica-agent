//! Read-only checks of the capabilities compiled into this worker executable.
use serde::Serialize;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Github,
    Alphaxiv,
    Scix,
    Overleaf,
}
#[derive(Clone, Debug)]
pub struct Options {
    pub only: Option<Service>,
    pub timeout: Duration,
    pub github_token_env: String,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            only: None,
            timeout: Duration::from_secs(30),
            github_token_env: "GH_TOKEN".into(),
        }
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Failed,
    Timeout,
    NotBuilt,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub service: &'static str,
    pub status: Status,
    pub message: String,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}
impl Report {
    pub fn success(&self) -> bool {
        self.checks.iter().all(|c| matches!(c.status, Status::Ok))
    }
}

/// Check selected services concurrently. No search, GitHub write, or permission mutation.
pub async fn run(options: &Options) -> Report {
    async fn selected(
        only: Option<Service>,
        service: Service,
        check: impl std::future::Future<Output = Check>,
    ) -> Option<Check> {
        if only.is_none() || only == Some(service) {
            Some(check.await)
        } else {
            None
        }
    }
    let (github, alphaxiv, scix, overleaf) = tokio::join!(
        selected(options.only, Service::Github, github(options)),
        selected(options.only, Service::Alphaxiv, alphaxiv(options)),
        selected(options.only, Service::Scix, scix(options)),
        selected(options.only, Service::Overleaf, overleaf(options)),
    );
    Report {
        checks: github
            .into_iter()
            .chain(alphaxiv)
            .chain(scix)
            .chain(overleaf)
            .collect(),
    }
}

#[cfg(any(
    feature = "github",
    feature = "alphaxiv",
    feature = "scix",
    feature = "overleaf",
    test
))]
async fn bounded(
    service: &'static str,
    timeout: Duration,
    future: impl std::future::Future<Output = Result<String, String>>,
) -> Check {
    match tokio::time::timeout(timeout, future).await {
        Ok(Ok(message)) => Check {
            service,
            status: Status::Ok,
            message,
        },
        Ok(Err(message)) => Check {
            service,
            status: Status::Failed,
            message,
        },
        Err(_) => Check {
            service,
            status: Status::Timeout,
            message: "Check timed out; verify connectivity or increase --timeout".into(),
        },
    }
}
#[cfg(not(all(
    feature = "github",
    feature = "alphaxiv",
    feature = "scix",
    feature = "overleaf"
)))]
fn not_built(service: &'static str, feature: &str) -> Check {
    Check {
        service,
        status: Status::NotBuilt,
        message: format!("Reinstall fridica-agent with --features {feature}"),
    }
}
async fn github(_options: &Options) -> Check {
    #[cfg(feature = "github")]
    {
        bounded("github", _options.timeout, async {
            crate::github::probe(&_options.github_token_env).await
                .map(|count| format!("Authenticated identity verified at api.github.com; {count} bundled tools available"))
                .map_err(|_| "GitHub check failed; verify the configured token variable or target-local gh auth login, and access to api.github.com".into())
        }).await
    }
    #[cfg(not(feature = "github"))]
    {
        not_built("github", "github")
    }
}
async fn alphaxiv(_options: &Options) -> Check {
    #[cfg(feature = "alphaxiv")]
    {
        bounded("alphaxiv", _options.timeout, async {
            crate::alphaxiv::probe().await
                .map(|tools| format!("Authenticated MCP at api.alphaxiv.org; {} research tools available; no search performed", tools.len()))
                .map_err(|_| "alphaXiv check failed; verify ALPHAXIV_TOKEN and access to api.alphaxiv.org".into())
        }).await
    }
    #[cfg(not(feature = "alphaxiv"))]
    {
        not_built("alphaxiv", "alphaxiv")
    }
}

async fn scix(_options: &Options) -> Check {
    #[cfg(feature = "scix")]
    {
        bounded("scix", _options.timeout, async {
            crate::scix::probe()
                .await
                .map(|count| format!("ADS accepted ADS_TOKEN; {count} tools"))
                .map_err(|err| err.to_string())
        })
        .await
    }
    #[cfg(not(feature = "scix"))]
    {
        not_built("scix", "scix")
    }
}

async fn overleaf(_options: &Options) -> Check {
    #[cfg(feature = "overleaf")]
    {
        bounded("overleaf", _options.timeout, async {
            crate::overleaf::probe()
                .await
                .map(|count| format!("interleaf {}: OVERLEAF_GIT_TOKEN set; {count} tools; remote project access untested", interleaf::VERSION))
                .map_err(|err| err.to_string())
        })
        .await
    }
    #[cfg(not(feature = "overleaf"))]
    {
        not_built("overleaf", "overleaf")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn result_statuses_and_deadlines_are_distinct() {
        assert!(matches!(
            bounded("test", Duration::from_secs(1), async { Ok("ready".into()) })
                .await
                .status,
            Status::Ok
        ));
        assert!(matches!(
            bounded("test", Duration::from_secs(1), async {
                Err("failed".into())
            })
            .await
            .status,
            Status::Failed
        ));
        assert!(matches!(
            bounded("test", Duration::from_millis(1), std::future::pending())
                .await
                .status,
            Status::Timeout
        ));
    }
}
