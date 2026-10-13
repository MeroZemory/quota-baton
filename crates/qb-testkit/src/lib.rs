pub struct FakeUpstream {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl FakeUpstream {
    pub async fn start() -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let task = tokio::spawn(async move {
            let app = axum::Router::new().fallback(|| async { "fake upstream" });
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { url, task })
    }
}

impl Drop for FakeUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn smoke() {
        let server = super::FakeUpstream::start().await.unwrap();
        assert_eq!(
            reqwest::get(&server.url)
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "fake upstream"
        );
    }
}
