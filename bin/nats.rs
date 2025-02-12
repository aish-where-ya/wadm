use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_nats::{
    jetstream::{
        self,
        kv::{Config as KvConfig, Store},
        stream::{Config as StreamConfig, Stream},
        Context,
    },
    Client, ConnectOptions,
};

use wadm::DEFAULT_EXPIRY_TIME;

/// Creates a NATS client from the given options
pub async fn get_client_and_context(
    url: String,
    js_domain: Option<String>,
    seed: Option<String>,
    jwt: Option<String>,
    creds_path: Option<PathBuf>,
) -> Result<(Client, Context)> {
    let client = if seed.is_none() && jwt.is_none() && creds_path.is_none() {
        async_nats::connect(url).await?
    } else {
        let opts = build_nats_options(seed, jwt, creds_path).await?;
        async_nats::connect_with_options(url, opts).await?
    };

    let context = if let Some(domain) = js_domain {
        jetstream::with_domain(client.clone(), domain)
    } else {
        jetstream::new(client.clone())
    };

    Ok((client, context))
}

async fn build_nats_options(
    seed: Option<String>,
    jwt: Option<String>,
    creds_path: Option<PathBuf>,
) -> Result<ConnectOptions> {
    match (seed, jwt, creds_path) {
        (Some(seed), Some(jwt), None) => {
            let jwt = resolve_jwt(jwt).await?;
            let kp = std::sync::Arc::new(get_seed(seed).await?);

            Ok(async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
                let key_pair = kp.clone();
                async move { key_pair.sign(&nonce).map_err(async_nats::AuthError::new) }
            }))
        }
        (None, None, Some(creds)) => async_nats::ConnectOptions::with_credentials_file(creds)
            .await
            .map_err(anyhow::Error::from),
        _ => {
            // We shouldn't ever get here due to the requirements on the flags, but return a helpful error just in case
            Err(anyhow::anyhow!(
                "Got too many options. Make sure to provide a seed and jwt or a creds path"
            ))
        }
    }
}

/// Takes a string that could be a raw seed, or a path and does all the necessary loading and parsing steps
async fn get_seed(seed: String) -> Result<nkeys::KeyPair> {
    // MAGIC NUMBER: Length of a seed key
    let raw_seed = if seed.len() == 58 && seed.starts_with('S') {
        seed
    } else {
        tokio::fs::read_to_string(seed).await?
    };

    nkeys::KeyPair::from_seed(&raw_seed).map_err(anyhow::Error::from)
}

/// Resolves a JWT value by either returning the string itself if it's a valid JWT
/// or by loading the contents of a file specified by the JWT value.
///
/// # Arguments
///
/// * `jwt_or_file` - A string that represents either a JWT or a file path containing a JWT.
///
/// # Returns
///
/// A `Result` containing a string if successful, or an error if the JWT value
/// is invalid or the file cannot be read.
async fn resolve_jwt(jwt_or_file: String) -> Result<String> {
    if tokio::fs::metadata(&jwt_or_file)
        .await
        .map(|metadata| metadata.is_file())
        .unwrap_or(false)
    {
        tokio::fs::read_to_string(jwt_or_file)
            .await
            .map_err(|e| anyhow!("Error loading JWT from file: {e}"))
    } else {
        // We could do more validation on the JWT here, but if the JWT is invalid then
        // connecting will fail anyways
        Ok(jwt_or_file)
    }
}

/// A helper that ensures that the given stream name exists, using defaults to create if it does
/// not. Returns the handle to the stream
pub async fn ensure_stream(
    context: &Context,
    name: String,
    subjects: Vec<String>,
    description: Option<String>,
) -> Result<Stream> {
    context
        .get_or_create_stream(StreamConfig {
            name,
            description,
            num_replicas: 1,
            retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
            subjects,
            max_age: DEFAULT_EXPIRY_TIME,
            storage: async_nats::jetstream::stream::StorageType::File,
            allow_rollup: false,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

pub async fn ensure_status_stream(
    context: &Context,
    name: String,
    subjects: Vec<String>,
) -> Result<Stream> {
    context
        .get_or_create_stream(StreamConfig {
            name,
            description: Some(
                "A stream that stores all status updates for wadm applications".into(),
            ),
            num_replicas: 1,
            allow_direct: true,
            retention: async_nats::jetstream::stream::RetentionPolicy::Limits,
            max_messages_per_subject: 10,
            subjects,
            max_age: std::time::Duration::from_nanos(0),
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// A helper that ensures that the notify stream exists
pub async fn ensure_notify_stream(
    context: &Context,
    name: String,
    subjects: Vec<String>,
) -> Result<Stream> {
    context
        .get_or_create_stream(StreamConfig {
            name,
            description: Some("A stream for capturing all notification events for wadm".into()),
            num_replicas: 1,
            retention: async_nats::jetstream::stream::RetentionPolicy::Interest,
            subjects,
            max_age: DEFAULT_EXPIRY_TIME,
            storage: async_nats::jetstream::stream::StorageType::File,
            ..Default::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))
}

/// A helper that ensures that the given KV bucket exists, using defaults to create if it does
/// not. Returns the handle to the stream
pub async fn ensure_kv_bucket(
    context: &Context,
    name: String,
    history_to_keep: i64,
) -> Result<Store> {
    if let Ok(kv) = context.get_key_value(&name).await {
        Ok(kv)
    } else {
        context
            .create_key_value(KvConfig {
                bucket: name,
                history: history_to_keep,
                num_replicas: 1,
                storage: jetstream::stream::StorageType::File,
                ..Default::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
    }
}

#[cfg(test)]
mod test {
    use super::resolve_jwt;
    use anyhow::Result;

    #[tokio::test]
    async fn can_resolve_jwt_value_and_file() -> Result<()> {
        let my_jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJ2aWRlb0lkIjoiUWpVaUxYSnVjMjl0IiwiaWF0IjoxNjIwNjAzNDY5fQ.2PKx6y2ym6IWbeM6zFgHOkDnZEtGTR3YgYlQ2_Jki5g";
        let jwt_path = "./test/data/nats.jwt";
        let jwt_inside_file = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdHJpbmciOiAiQWNjb3JkIHRvIGFsbCBrbm93biBsb3dzIG9mIGF2aWF0aW9uLCB0aGVyZSBpcyBubyB3YXkgdGhhdCBhIGJlZSBhYmxlIHRvIGZseSJ9.GyU6pTRhflcOg6KBCU6wZedP8BQzLXbdgYIoU6KzzD8";

        assert_eq!(
            resolve_jwt(my_jwt.to_string())
                .await
                .expect("should resolve jwt string to itself"),
            my_jwt.to_string()
        );
        assert_eq!(
            resolve_jwt(jwt_path.to_string())
                .await
                .expect("should be able to read jwt file"),
            jwt_inside_file.to_string()
        );

        Ok(())
    }
}
