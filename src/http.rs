//! HTTP client construction shared by the Notion, auth and updater clients.
//!
//! On a network that inspects HTTPS (a corporate proxy), servers present certificates issued by a
//! private root. Besides the bundled and OS trust stores, clients built here trust the PEM
//! certificates named by `NOTION_EXTRA_CA_PEM`, `SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS`.
//!
//! Nothing here writes to stdout: `notion mcp` speaks JSON-RPC on it.

use std::{error::Error as StdError, path::PathBuf, sync::OnceLock};

use anyhow::{anyhow, bail, Context, Result};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use serde::Serialize;

use crate::models::coded;

/// The variable owned by this tool: a PEM file path, or the PEM text itself.
pub const EXTRA_CA_ENV: &str = "NOTION_EXTRA_CA_PEM";
/// Variables owned by other tools, read as PEM file paths.
const SHARED_CA_ENVS: [&str; 2] = ["SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"];

/// Shown when a request fails because the server's certificate issuer is not trusted.
pub const TLS_TRUST_HINT: &str = "TLS certificate issuer is not trusted. If your network inspects HTTPS (corporate proxy),\n\
export the proxy's root CA as PEM and set NOTION_EXTRA_CA_PEM=<path-to-ca.pem>\n\
(SSL_CERT_FILE and NODE_EXTRA_CA_CERTS are also honoured), then retry.";

#[derive(Debug, Clone, PartialEq, Eq)]
enum CaOrigin {
    Inline(String),
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CaSource {
    variable: &'static str,
    origin: CaOrigin,
}

impl CaSource {
    fn describe(&self) -> String {
        match &self.origin {
            CaOrigin::Inline(_) => "inline PEM".to_string(),
            CaOrigin::File(path) => path.display().to_string(),
        }
    }
}

/// What one variable contributed, as shown by `notion doctor`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaReport {
    pub variable: &'static str,
    /// The file path, or "inline PEM".
    pub source: String,
    /// Certificates now trusted from this source.
    pub certificates: usize,
    /// Certificates in the source that cannot be used as a trust root and were left out.
    pub skipped: usize,
    /// Why the source was not loaded at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Root certificates loaded from the environment, and where each came from.
#[derive(Default)]
pub struct ExtraCas {
    certificates: Vec<reqwest::Certificate>,
    pub reports: Vec<CaReport>,
}

impl ExtraCas {
    fn apply(&self, mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        for certificate in &self.certificates {
            builder = builder.add_root_certificate(certificate.clone());
        }
        builder
    }
}

/// The sources that are set, in load order. All of them are loaded; none replaces another.
/// The reader is passed in so tests do not change the process environment.
fn extra_ca_sources(env: impl Fn(&str) -> Option<String>) -> Vec<CaSource> {
    let mut sources = Vec::new();
    for variable in std::iter::once(EXTRA_CA_ENV).chain(SHARED_CA_ENVS) {
        let Some(value) = env(variable) else { continue };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let origin = if variable == EXTRA_CA_ENV && value.starts_with("-----BEGIN") {
            CaOrigin::Inline(value.to_string())
        } else {
            CaOrigin::File(PathBuf::from(value))
        };
        sources.push(CaSource { variable, origin });
    }
    sources
}

/// Reads the usable certificates of one source and counts the ones left out.
///
/// reqwest fails the whole client build on a root certificate rustls cannot use, and bundles
/// copied from an OS store often hold a few of those, so each certificate is checked here first.
fn read_source(source: &CaSource) -> Result<(Vec<CertificateDer<'static>>, usize)> {
    let pem = match &source.origin {
        CaOrigin::Inline(text) => text.clone().into_bytes(),
        CaOrigin::File(path) => std::fs::read(path).context("cannot read the file")?,
    };
    let mut usable = Vec::new();
    let mut skipped = 0usize;
    for item in CertificateDer::pem_slice_iter(&pem) {
        let der = item.map_err(|err| anyhow!("not valid PEM ({err})"))?;
        if rustls::RootCertStore::empty().add(der.clone()).is_ok() {
            usable.push(der);
        } else {
            skipped += 1;
        }
    }
    if usable.is_empty() {
        if skipped > 0 {
            bail!("none of its {skipped} certificate(s) can be used as a trust root");
        }
        bail!("no PEM certificate found");
    }
    Ok((usable, skipped))
}

/// Loads every source. A source this tool owns must load; one owned by another tool is reported
/// in [`CaReport::error`] and skipped.
fn load_extra_cas(sources: &[CaSource]) -> Result<ExtraCas> {
    let mut loaded = ExtraCas::default();
    for source in sources {
        let mut report = CaReport {
            variable: source.variable,
            source: source.describe(),
            certificates: 0,
            skipped: 0,
            error: None,
        };
        match read_source(source) {
            Ok((certificates, skipped)) => {
                report.certificates = certificates.len();
                report.skipped = skipped;
                for der in certificates {
                    loaded.certificates.push(
                        reqwest::Certificate::from_der(&der)
                            .with_context(|| format!("{} holds an unusable certificate", source.variable))?,
                    );
                }
            }
            Err(err) if source.variable == EXTRA_CA_ENV => {
                bail!(
                    "{EXTRA_CA_ENV} ({}) could not be loaded: {err:#}. Point it at a PEM certificate file, or unset it.",
                    report.source
                );
            }
            Err(err) => report.error = Some(format!("{err:#}")),
        }
        loaded.reports.push(report);
    }
    Ok(loaded)
}

/// The certificates named by the process environment, loaded once. A source that was skipped is
/// reported on stderr the first time.
pub fn extra_cas() -> Result<&'static ExtraCas> {
    static LOADED: OnceLock<Result<ExtraCas, String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let loaded = load_extra_cas(&extra_ca_sources(|name| std::env::var(name).ok()))
                .map_err(|err| format!("{err:#}"))?;
            for report in &loaded.reports {
                if let Some(error) = &report.error {
                    eprintln!(
                        "warning: ignoring {} ({}): {error}",
                        report.variable, report.source
                    );
                }
            }
            Ok(loaded)
        })
        .as_ref()
        .map_err(|message| coded("invalid_input", message.clone()))
}

/// A client builder that also trusts the extra root certificates. Fails when
/// `NOTION_EXTRA_CA_PEM` is set but cannot be loaded.
pub fn client_builder() -> Result<reqwest::ClientBuilder> {
    Ok(extra_cas()?.apply(reqwest::Client::builder()))
}

fn is_unknown_issuer(error: &rustls::Error) -> bool {
    matches!(
        error,
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
    )
}

/// Returns [`TLS_TRUST_HINT`] when `err`, or an error it wraps, is an untrusted-issuer failure.
pub fn tls_trust_hint(err: &(dyn StdError + 'static)) -> Option<&'static str> {
    let mut current = Some(err);
    while let Some(error) = current {
        let direct = error.downcast_ref::<rustls::Error>();
        // An io::Error reports its inner error's source, not the inner error, so look inside it.
        let inside_io = error
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .and_then(|inner| inner.downcast_ref::<rustls::Error>());
        if direct.or(inside_io).is_some_and(is_unknown_issuer)
            || error.to_string().contains("UnknownIssuer")
        {
            return Some(TLS_TRUST_HINT);
        }
        current = error.source();
    }
    None
}

/// Turns a failed request into an error whose message is `message`, followed by the trust hint
/// when the cause is an untrusted issuer.
pub fn transport_error(err: reqwest::Error, message: impl Into<String>) -> anyhow::Error {
    let mut message = message.into();
    if let Some(hint) = tls_trust_hint(&err) {
        message.push('\n');
        message.push_str(hint);
    }
    anyhow::Error::new(err).context(message)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    // Self-signed test roots (certificates only), valid until 2126.
    const TEST_ROOT_A: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBmTCCAT+gAwIBAgIUQASxcLqbTK53zimIfCDkEgcQVVYwCgYIKoZIzj0EAwIw\n\
ITEfMB0GA1UEAwwWbm90aW9uLWNsaSB0ZXN0IHJvb3QgQTAgFw0yNjExMjMwNzQ5\n\
MjBaGA8yMTI2MTAzMDA3NDkyMFowITEfMB0GA1UEAwwWbm90aW9uLWNsaSB0ZXN0\n\
IHJvb3QgQTBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABJTdKJFfrlLddi2yoryy\n\
f4LZfwrYvGwq/ggIi8Js817u4wWu+UNALkBzVcSSVuGY6yrZ9Ja5879mvyqTB89t\n\
RrijUzBRMB0GA1UdDgQWBBSm/pSbv10mVdLRjg4CG+j5RVyYIjAfBgNVHSMEGDAW\n\
gBSm/pSbv10mVdLRjg4CG+j5RVyYIjAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49\n\
BAMCA0gAMEUCIQDHR+HQTfDha4hKFHoAVWqy6mocd+7KS0t6LX1rBFnx1AIgNSaa\n\
Lenw0Rn7IX0blcdybNoeWcybVI6xMmOqIP+PSZ0=\n\
-----END CERTIFICATE-----\n";
    const TEST_ROOT_B: &str = "-----BEGIN CERTIFICATE-----\n\
MIIBmDCCAT+gAwIBAgIUM0VSwYfItuHvPQR+VnOEHJeedXcwCgYIKoZIzj0EAwIw\n\
ITEfMB0GA1UEAwwWbm90aW9uLWNsaSB0ZXN0IHJvb3QgQjAgFw0yNjExMjMwNzQ5\n\
MjBaGA8yMTI2MTAzMDA3NDkyMFowITEfMB0GA1UEAwwWbm90aW9uLWNsaSB0ZXN0\n\
IHJvb3QgQjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABNkw5xb6RK7jU96F2WV8\n\
CgdyA/uoIpPd493r8AWLI6b+glLlvzY9KMyJwWTplcjQer46TOzUZwA9ilyn44Un\n\
vkyjUzBRMB0GA1UdDgQWBBS2Gj9XimD3VTGPa6S0vuQ2L+SRqzAfBgNVHSMEGDAW\n\
gBS2Gj9XimD3VTGPa6S0vuQ2L+SRqzAPBgNVHRMBAf8EBTADAQH/MAoGCCqGSM49\n\
BAMCA0cAMEQCIEaX+qVJDf1MKJd2ZGMhP9KhVkFxVJz19Bjiut4NDdiFAiBhJMIC\n\
ML51SCZp6W9ArN6b813CPGlfCIJiMUsrQFjm5Q==\n\
-----END CERTIFICATE-----\n";

    fn fake_env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        }
    }

    fn pem_file(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    fn file_source(variable: &'static str, file: &tempfile::NamedTempFile) -> CaSource {
        CaSource { variable, origin: CaOrigin::File(file.path().to_path_buf()) }
    }

    #[test]
    fn sources_are_additive_and_ordered_and_blank_values_are_ignored() {
        let all = extra_ca_sources(fake_env(&[
            ("NODE_EXTRA_CA_CERTS", "/etc/node-ca.pem"),
            ("SSL_CERT_FILE", " /etc/ssl/cert.pem "),
            ("NOTION_EXTRA_CA_PEM", "/etc/notion-ca.pem"),
        ]));
        assert_eq!(
            all.iter().map(|s| s.variable).collect::<Vec<_>>(),
            vec!["NOTION_EXTRA_CA_PEM", "SSL_CERT_FILE", "NODE_EXTRA_CA_CERTS"]
        );
        assert_eq!(all[1].origin, CaOrigin::File(PathBuf::from("/etc/ssl/cert.pem")));

        let some = extra_ca_sources(fake_env(&[
            ("NOTION_EXTRA_CA_PEM", ""),
            ("SSL_CERT_FILE", "   "),
            ("NODE_EXTRA_CA_CERTS", "ca.pem"),
        ]));
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].variable, "NODE_EXTRA_CA_CERTS");
        assert!(extra_ca_sources(fake_env(&[])).is_empty());
    }

    #[test]
    fn only_the_notion_variable_accepts_inline_pem() {
        let inline = extra_ca_sources(fake_env(&[("NOTION_EXTRA_CA_PEM", TEST_ROOT_A)]));
        assert!(matches!(inline[0].origin, CaOrigin::Inline(_)));
        assert_eq!(inline[0].describe(), "inline PEM");
        let path = extra_ca_sources(fake_env(&[("NOTION_EXTRA_CA_PEM", "C:\\certs\\ca.pem")]));
        assert_eq!(path[0].origin, CaOrigin::File(PathBuf::from("C:\\certs\\ca.pem")));
        let shared = extra_ca_sources(fake_env(&[("SSL_CERT_FILE", TEST_ROOT_A)]));
        assert!(matches!(shared[0].origin, CaOrigin::File(_)));

        let loaded = load_extra_cas(&inline).unwrap();
        assert_eq!(loaded.reports[0].certificates, 1);
    }

    #[test]
    fn a_bundle_file_yields_every_certificate_and_the_client_builds() {
        let bundle = pem_file(&format!("{TEST_ROOT_A}{TEST_ROOT_B}"));
        let single = pem_file(TEST_ROOT_B);
        let loaded = load_extra_cas(&[
            file_source("NOTION_EXTRA_CA_PEM", &bundle),
            file_source("NODE_EXTRA_CA_CERTS", &single),
        ])
        .unwrap();
        assert_eq!(
            loaded.reports.iter().map(|r| (r.variable, r.certificates, r.skipped)).collect::<Vec<_>>(),
            vec![("NOTION_EXTRA_CA_PEM", 2, 0), ("NODE_EXTRA_CA_CERTS", 1, 0)]
        );
        assert!(loaded.reports.iter().all(|r| r.error.is_none()));
        assert_eq!(loaded.certificates.len(), 3);
        loaded.apply(reqwest::Client::builder()).build().unwrap();
    }

    #[test]
    fn a_bad_notion_source_is_an_error_and_a_bad_shared_source_is_skipped() {
        let garbage = pem_file("this is not a certificate\n");
        let err = load_extra_cas(&[file_source("NOTION_EXTRA_CA_PEM", &garbage)])
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("NOTION_EXTRA_CA_PEM"), "{err}");
        assert!(err.contains(&garbage.path().display().to_string()), "{err}");
        assert!(err.contains("no PEM certificate found"), "{err}");

        let missing = CaSource {
            variable: "NOTION_EXTRA_CA_PEM",
            origin: CaOrigin::File(PathBuf::from("does-not-exist/ca.pem")),
        };
        let err = load_extra_cas(&[missing]).err().unwrap().to_string();
        assert!(err.contains("cannot read the file"), "{err}");

        let good = pem_file(TEST_ROOT_A);
        let loaded = load_extra_cas(&[
            file_source("SSL_CERT_FILE", &garbage),
            CaSource {
                variable: "NODE_EXTRA_CA_CERTS",
                origin: CaOrigin::File(PathBuf::from("does-not-exist/ca.pem")),
            },
            file_source("NOTION_EXTRA_CA_PEM", &good),
        ])
        .unwrap();
        assert_eq!(loaded.reports[0].error.as_deref(), Some("no PEM certificate found"));
        assert!(loaded.reports[1].error.as_deref().unwrap().starts_with("cannot read the file"));
        assert_eq!(loaded.reports[2].certificates, 1);
        assert_eq!(loaded.certificates.len(), 1, "skipped sources add nothing");
    }

    #[test]
    fn certificates_rustls_cannot_use_are_left_out_without_failing_the_client() {
        let broken = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
        let mixed = pem_file(&format!("{broken}{TEST_ROOT_A}"));
        let loaded = load_extra_cas(&[file_source("SSL_CERT_FILE", &mixed)]).unwrap();
        assert_eq!((loaded.reports[0].certificates, loaded.reports[0].skipped), (1, 1));
        loaded.apply(reqwest::Client::builder()).build().unwrap();

        let only_broken = pem_file(broken);
        let loaded = load_extra_cas(&[file_source("SSL_CERT_FILE", &only_broken)]).unwrap();
        assert!(loaded.reports[0].error.as_deref().unwrap().contains("trust root"));
        assert!(load_extra_cas(&[file_source("NOTION_EXTRA_CA_PEM", &only_broken)]).is_err());
    }

    #[test]
    fn trust_hint_finds_an_unknown_issuer_under_context_layers() {
        let unknown_issuer =
            || rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let wrapped = anyhow::Error::new(unknown_issuer())
            .context("error sending request")
            .context("Notion API request failed: GET https://api.notion.com/v1/users/me");
        let as_std: &(dyn StdError + 'static) = wrapped.as_ref();
        assert_eq!(tls_trust_hint(as_std), Some(TLS_TRUST_HINT));

        // The shape a real connection failure has: rustls inside an io::Error.
        let io = std::io::Error::new(std::io::ErrorKind::InvalidData, unknown_issuer());
        let wrapped = anyhow::Error::new(io).context("client error (Connect)");
        let as_std: &(dyn StdError + 'static) = wrapped.as_ref();
        assert_eq!(tls_trust_hint(as_std), Some(TLS_TRUST_HINT));

        let opaque = anyhow!("invalid peer certificate: UnknownIssuer").context("connect");
        let as_std: &(dyn StdError + 'static) = opaque.as_ref();
        assert_eq!(tls_trust_hint(as_std), Some(TLS_TRUST_HINT));
    }

    #[test]
    fn trust_hint_is_absent_for_other_failures() {
        let timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "operation timed out");
        let wrapped = anyhow::Error::new(timeout).context("Notion API request failed");
        let as_std: &(dyn StdError + 'static) = wrapped.as_ref();
        assert_eq!(tls_trust_hint(as_std), None);

        let expired = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        assert_eq!(tls_trust_hint(&expired), None);
    }
}
