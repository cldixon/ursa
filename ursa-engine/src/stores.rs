//! Object stores, registered the first time a query names a URL.
//!
//! DataFusion only knows the local filesystem until a store is registered for a
//! URL's `scheme://authority`. As datafusion-cli does, the engine registers one
//! lazily: when a query first reads (or `COPY`s to) `s3://bucket/…`, a store for
//! `s3://bucket` is built from the standard environment and kept for the
//! session.
//!
//! Credentials come from each backend's environment variables and default
//! credential chain (`object_store`'s `from_env`): `AWS_*` (plus web identity,
//! container and instance credentials), `GOOGLE_*` (plus application default
//! credentials), and `AZURE_*`. A public S3 bucket can be read with
//! `AWS_SKIP_SIGNATURE=true`.
//!
//! With the `aws-profile` feature (on by default in the `ursa` binary), S3
//! also reads the AWS profile files, `~/.aws/config` and `~/.aws/credentials`
//! (or `AWS_CONFIG_FILE` and `AWS_SHARED_CREDENTIALS_FILE`), for the profile
//! `AWS_PROFILE` names, or `default`. It comes after the environment's keys
//! and before web identity, container and instance credentials, the order the
//! AWS CLI uses. A profile can hold static keys, an SSO login, a role to
//! assume or a `credential_process`; `aws-config` resolves each as the AWS
//! SDKs do. See [`aws_profile`].

use std::sync::Arc;

use datafusion::error::{DataFusionError, Result};
use datafusion::execution::runtime_env::RuntimeEnv;
use object_store::aws::{resolve_bucket_region, AmazonS3Builder};
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::GoogleCloudStorageBuilder;
use object_store::http::HttpBuilder;
use object_store::{ClientOptions, ObjectStore};
use url::{Position, Url};

/// Make sure `runtime` has an object store for `url`, building and registering
/// one if this is the first time the session sees its `scheme://authority`.
/// Local paths need none.
pub async fn ensure_object_store(runtime: &RuntimeEnv, url: &Url) -> Result<()> {
    if url.scheme() == "file" || runtime.object_store_registry.get_store(url).is_ok() {
        return Ok(());
    }
    let store = build_object_store(url).await?;
    runtime.register_object_store(&store_base(url)?, store);
    Ok(())
}

/// [`ensure_object_store`] for a location written in SQL (a `COPY … TO` target
/// or an external table's `LOCATION`). A local path needs no store.
pub async fn ensure_object_store_for(runtime: &RuntimeEnv, location: &str) -> Result<()> {
    match Url::parse(location) {
        // A one-letter scheme is a Windows drive letter.
        Ok(url) if url.scheme().len() > 1 => ensure_object_store(runtime, &url).await,
        _ => Ok(()),
    }
}

/// The `scheme://authority[:port]` a store for `url` is registered under.
fn store_base(url: &Url) -> Result<Url> {
    Url::parse(&url[..Position::BeforePath])
        .map_err(|e| DataFusionError::Plan(format!("'{url}' is not a valid URL: {e}")))
}

/// A store for `url`'s scheme and bucket, configured from the environment.
pub async fn build_object_store(url: &Url) -> Result<Arc<dyn ObjectStore>> {
    let base = &url[..Position::BeforePath];
    let failed = |e: object_store::Error| {
        DataFusionError::Execution(format!("cannot connect to {base}: {e}"))
    };
    let store: Arc<dyn ObjectStore> = match url.scheme() {
        "s3" | "s3a" => {
            let mut builder = AmazonS3Builder::from_env().with_url(url.as_str());
            // The region a profile names, used when the environment names
            // none and S3 cannot say where the bucket is.
            #[allow(unused_mut)]
            let mut profile_region: Option<String> = None;
            #[cfg(feature = "aws-profile")]
            if let Some(profile) = aws_profile::load()
                .await
                .map_err(|e| DataFusionError::Execution(format!("cannot connect to {base}: {e}")))?
            {
                if let Some(credentials) = profile.credentials {
                    builder = builder.with_credentials(credentials);
                }
                profile_region = profile.region;
            }
            if !env_set(&["AWS_REGION", "AWS_DEFAULT_REGION"]) {
                // Without a region the client assumes us-east-1, and any other
                // bucket answers with a redirect. Ask S3 where the bucket is,
                // unless the environment names an endpoint (a local or
                // S3-compatible store), then fall back to the profile's region.
                let mut region = None;
                if !env_set(&["AWS_ENDPOINT", "AWS_ENDPOINT_URL"]) {
                    if let Some(bucket) = url.host_str() {
                        region = resolve_bucket_region(bucket, &ClientOptions::new())
                            .await
                            .ok();
                    }
                }
                if let Some(region) = region.or(profile_region) {
                    builder = builder.with_region(region);
                }
            }
            Arc::new(builder.build().map_err(failed)?)
        }
        "gs" | "gcs" => Arc::new(
            GoogleCloudStorageBuilder::from_env()
                .with_url(url.as_str())
                .build()
                .map_err(failed)?,
        ),
        "az" | "azure" | "abfs" | "abfss" | "adl" => Arc::new(
            MicrosoftAzureBuilder::from_env()
                .with_url(url.as_str())
                .build()
                .map_err(failed)?,
        ),
        // A plain HTTP(S) URL reads single files with ranged GETs. `http://` is
        // allowed because the user chose the scheme.
        "http" | "https" => Arc::new(
            HttpBuilder::new()
                .with_url(base)
                .with_client_options(ClientOptions::new().with_allow_http(true))
                .build()
                .map_err(failed)?,
        ),
        other => {
            return Err(DataFusionError::Plan(format!(
                "Ursa cannot read '{other}://' URLs; use a local path or an s3://, gs://, az://, \
                 http:// or https:// URL"
            )))
        }
    };
    Ok(store)
}

/// Whether any of the environment variables `keys` is set and not empty.
fn env_set(keys: &[&str]) -> bool {
    keys.iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// S3 credentials and region from the AWS profile files, as the AWS SDKs read
/// them (`aws-config`).
///
/// The profile is `AWS_PROFILE`'s, or `default`. It is read only when the
/// environment holds no keys (`AWS_ACCESS_KEY_ID`) and does not ask to skip
/// signing (`AWS_SKIP_SIGNATURE`), so the environment wins, as it does for the
/// AWS CLI. A profile with no credentials (none of static keys, SSO, a role or
/// a `credential_process`) leaves the rest of `object_store`'s chain to find
/// them: web identity, container and instance credentials. A profile that
/// `AWS_PROFILE` names and that cannot give credentials is an error, rather
/// than a quiet fall back to other credentials.
#[cfg(feature = "aws-profile")]
pub mod aws_profile {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use async_trait::async_trait;
    use aws_config::meta::region::ProvideRegion;
    use aws_config::profile::{ProfileFileCredentialsProvider, ProfileFileRegionProvider};
    use aws_config::provider_config::ProviderConfig;
    use aws_credential_types::provider::error::CredentialsError;
    use aws_credential_types::provider::ProvideCredentials;
    use aws_credential_types::Credentials;
    use aws_smithy_http_client::tls::{self, rustls_provider::CryptoMode};
    use object_store::aws::AwsCredential;
    use object_store::CredentialProvider;
    use tokio::sync::Mutex;

    /// What a profile gives S3.
    pub struct Profile {
        /// Its credentials, or `None` when it has none.
        pub credentials: Option<Arc<ProfileCredentials>>,
        pub region: Option<String>,
    }

    /// The profile's credentials and region, or `None` when the environment
    /// supplies credentials itself; an error says what is wrong with the
    /// profile.
    pub async fn load() -> Result<Option<Profile>, String> {
        let skip_signature = std::env::var("AWS_SKIP_SIGNATURE")
            .is_ok_and(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "1"));
        if super::env_set(&["AWS_ACCESS_KEY_ID"]) || skip_signature {
            return Ok(None);
        }
        let named = std::env::var("AWS_PROFILE").ok().filter(|p| !p.is_empty());
        let name = named.clone().unwrap_or_else(|| "default".to_string());
        // SSO and assume-role reach AWS over HTTPS; ring is the TLS crypto the
        // rest of the tree already uses.
        let http = aws_smithy_http_client::Builder::new()
            .tls_provider(tls::Provider::Rustls(CryptoMode::Ring))
            .build_https();
        let config = ProviderConfig::without_region().with_http_client(http);
        let region = ProfileFileRegionProvider::builder()
            .profile_name(&name)
            .configure(&config)
            .build()
            .region()
            .await;
        let config = config.with_region(region.clone());
        let provider = ProfileFileCredentialsProvider::builder()
            .profile_name(&name)
            .configure(&config)
            .build();
        let region = region.map(|r| r.to_string());
        match provider.provide_credentials().await {
            Ok(first) => Ok(Some(Profile {
                credentials: Some(Arc::new(ProfileCredentials {
                    provider,
                    current: Mutex::new(first),
                })),
                region,
            })),
            Err(CredentialsError::CredentialsNotLoaded(_)) if named.is_none() => {
                Ok(Some(Profile {
                    credentials: None,
                    region,
                }))
            }
            Err(e) => Err(format!(
                "AWS profile '{name}' gives no credentials: {}",
                error_chain(&e)
            )),
        }
    }

    /// `e` and its sources, joined, as `aws-config` words them.
    fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
        let mut parts = vec![e.to_string()];
        let mut source = e.source();
        while let Some(s) = source {
            let text = s.to_string();
            if !parts.iter().any(|p| p.contains(&text)) {
                parts.push(text);
            }
            source = s.source();
        }
        parts.join(": ")
    }

    /// A profile's credentials for `object_store`, refreshed shortly before
    /// they expire (an SSO login or an assumed role's are temporary).
    #[derive(Debug)]
    pub struct ProfileCredentials {
        provider: ProfileFileCredentialsProvider,
        current: Mutex<Credentials>,
    }

    /// How long before expiry credentials are refreshed.
    const REFRESH_BEFORE: Duration = Duration::from_secs(5 * 60);

    #[async_trait]
    impl CredentialProvider for ProfileCredentials {
        type Credential = AwsCredential;

        async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
            let mut current = self.current.lock().await;
            let stale = current
                .expiry()
                .is_some_and(|at| at <= SystemTime::now() + REFRESH_BEFORE);
            if stale {
                *current = self.provider.provide_credentials().await.map_err(|e| {
                    object_store::Error::Generic {
                        store: "S3",
                        source: error_chain(&e).into(),
                    }
                })?;
            }
            Ok(Arc::new(AwsCredential {
                key_id: current.access_key_id().to_string(),
                secret_key: current.secret_access_key().to_string(),
                token: current.session_token().map(str::to_string),
            }))
        }
    }
}
