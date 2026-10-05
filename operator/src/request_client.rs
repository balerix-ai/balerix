//! The kube `Client` the reconciles run on (Spec O §22.1): kube's own
//! stack with a bound on every request, so a request lost on a pooled
//! connection (hyperium/hyper#4207, #129) fails after `REQUEST_TIMEOUT`
//! instead of waiting forever. The bound covers the response head; kube
//! reads the body after it. Watches never run on this client: they get
//! `watch_client`'s, where a bound would cut an idle watch.

use std::time::Duration;

use kube::{Client, Config};

/// Well inside `controllers::RECONCILE_TIMEOUT` (30 s): a lost request
/// fails its reconcile before the reconcile bound has to cut it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub fn request_client(config: Config) -> kube::Result<Client> {
    bounded_client(config, REQUEST_TIMEOUT)
}

fn bounded_client(config: Config, limit: Duration) -> kube::Result<Client> {
    Ok(kube::client::ClientBuilder::try_from(config)?
        .with_layer(&tower::timeout::TimeoutLayer::new(limit))
        .build())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use k8s_openapi::api::core::v1::ConfigMap;

    #[tokio::test]
    async fn a_request_that_gets_no_answer_fails_within_the_bound() {
        // accepts and holds every connection, reads nothing, answers nothing
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = tokio::spawn(async move {
            let mut sockets = Vec::new();
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                sockets.push(socket);
            }
        });
        let client = bounded_client(
            Config::new(url.parse().unwrap()),
            Duration::from_millis(300),
        )
        .unwrap();
        let api: kube::Api<ConfigMap> = kube::Api::namespaced(client, "ns");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(10), api.get("c"))
            .await
            .expect("the request was not bounded")
            .unwrap_err();
        assert!(
            matches!(&error, kube::Error::Service(e) if e.is::<tower::timeout::error::Elapsed>()),
            "{error:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        held.abort();
    }
}
