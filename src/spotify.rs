use crate::{auth::Auth, database::now};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use rand::Rng;
use serde::Deserialize;
use serde_json::Value;
use sqlx::SqlitePool;
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

#[derive(Debug)]
pub struct Paused {
    pub message: String,
    pub retry_at: Option<DateTime<Utc>>,
}

impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}\n{}",
            crate::rate_limit::local_notice(self.retry_at),
            self.message
        )
    }
}

impl std::error::Error for Paused {}

#[derive(Debug, Error)]
#[error("Gravação rejeitada antes de ser aplicada")]
pub struct WriteRejected;

#[derive(Debug, Error)]
#[error("Orçamento de requisições esgotado; progresso preservado. Continue com sync --resume ou aumente o orçamento explicitamente.")]
pub struct BudgetExhausted;

#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub items: Vec<Value>,
    pub offset: i64,
    pub total: i64,
    pub next: Option<String>,
}
impl Page {
    pub fn validate(&self, expected_offset: i64, expected_total: Option<i64>) -> Result<bool> {
        if self.offset != expected_offset
            || self.total < 0
            || self.items.len() > 50
            || expected_total.is_some_and(|t| self.total != t)
        {
            bail!("Paginação inconsistente: biblioteca alterada ou resposta inválida; execute sync --resume");
        }
        let end = self.offset + self.items.len() as i64;
        if end > self.total
            || (self.items.is_empty() && end < self.total)
            || (self.next.is_some() != (end < self.total))
        {
            bail!("Paginação incompleta: reconciliação bloqueada");
        }
        Ok(end == self.total)
    }
}

#[async_trait]
pub trait Api: Send {
    async fn get(&mut self, path: &str, offset: Option<i64>) -> Result<Value>;
    async fn contains(&mut self, _uris: &[String]) -> Result<Value> {
        bail!("Consulta contains não suportada por este cliente")
    }
    async fn followed_artists(&mut self, _after: Option<&str>) -> Result<Value> {
        bail!("Consulta de artistas seguidos não suportada")
    }
    async fn artist_albums(
        &mut self,
        _artist: &str,
        _offset: i64,
        _groups: &str,
        _market: Option<&str>,
    ) -> Result<Value> {
        bail!("Consulta de discografia não suportada")
    }
    async fn album_tracks(
        &mut self,
        album: &str,
        offset: i64,
        _market: Option<&str>,
    ) -> Result<Value> {
        self.get(&format!("/albums/{album}/tracks"), Some(offset))
            .await
    }
    fn set_budget(&mut self, _remaining: Option<u32>) {}
    fn attempt_count(&self) -> u64 {
        0
    }
}

pub struct Spotify {
    pub auth: Auth,
    http: reqwest::Client,
    base_url: String,
    interval: Duration,
    last_request: Option<Instant>,
    budget_remaining: Option<u32>,
    attempts: u64,
    pub metrics: Option<(SqlitePool, String)>,
}
impl Spotify {
    pub fn new(auth: Auth, interval_ms: u64) -> Result<Self> {
        Ok(Self {
            auth,
            base_url: "https://api.spotify.com/v1".into(),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(45))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            interval: Duration::from_millis(interval_ms),
            last_request: None,
            budget_remaining: None,
            attempts: 0,
            metrics: None,
        })
    }
    async fn backoff(&self, attempt: u32) -> Result<()> {
        self.count(true).await?;
        let jitter = rand::thread_rng().gen_range(0..500);
        tokio::time::sleep(Duration::from_millis((1u64 << attempt) * 1000 + jitter)).await;
        Ok(())
    }
    async fn count(&self, retry: bool) -> Result<()> {
        if let Some((pool, job)) = &self.metrics {
            sqlx::query("UPDATE sync_job SET request_count=request_count+?,retry_count=retry_count+?,updated_at=? WHERE id=?")
                .bind(if retry {0} else {1}).bind(if retry {1} else {0}).bind(now()).bind(job).execute(pool).await?;
        }
        Ok(())
    }
}

pub fn retry_after(header: Option<&str>) -> Duration {
    header
        .and_then(|s| {
            s.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                httpdate::parse_http_date(s)
                    .ok()
                    .map(|date| date.duration_since(SystemTime::now()).unwrap_or_default())
            })
        })
        .unwrap_or(Duration::from_secs(60))
}
pub fn quota_exceeded(body: &Value) -> bool {
    body.pointer("/error/reason")
        .or_else(|| body.pointer("/error/code"))
        .or_else(|| body.get("reason"))
        .and_then(Value::as_str)
        .is_some_and(|s| s.eq_ignore_ascii_case("QUOTA_EXCEEDED"))
        || body
            .to_string()
            .to_ascii_uppercase()
            .contains("QUOTA_EXCEEDED")
}

impl Spotify {
    pub(crate) async fn write_request(&mut self, path: &str, body: &Value) -> Result<Value> {
        self.request_method(reqwest::Method::POST, path, &[], Some(body))
            .await
    }
    pub(crate) async fn request(
        &mut self,
        path: &str,
        parameters: &[(&str, String)],
    ) -> Result<Value> {
        self.request_method(reqwest::Method::GET, path, parameters, None)
            .await
    }
    async fn request_method(
        &mut self,
        method: reqwest::Method,
        path: &str,
        parameters: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let writing = method != reqwest::Method::GET;
        // Never follow API-supplied URLs with Authorization credentials.
        if !path.starts_with('/')
            || path.contains("://")
            || path.contains('?')
            || path.contains('#')
        {
            bail!("Caminho de API inválido");
        }
        let mut refreshed = false;
        for attempt in 0..=4u32 {
            if let Some(last) = self.last_request {
                tokio::time::sleep(self.interval.saturating_sub(last.elapsed())).await;
            }
            if let Some(remaining) = &mut self.budget_remaining {
                if *remaining == 0 {
                    return Err(BudgetExhausted.into());
                }
                *remaining -= 1;
            }
            let token = self.auth.access_token().await?.to_owned();
            self.attempts += 1;
            self.count(false).await?;
            self.last_request = Some(Instant::now());
            let mut request = self
                .http
                .request(method.clone(), format!("{}{path}", self.base_url))
                .bearer_auth(token);
            request = request.query(parameters);
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await;
            if let Ok(response) = response {
                let status = response.status();
                if status == reqwest::StatusCode::UNAUTHORIZED && !refreshed {
                    refreshed = true;
                    if let Err(error) = self.auth.refresh().await {
                        return Err(if writing {
                            error.context(WriteRejected)
                        } else {
                            error
                        });
                    }
                    self.count(true).await?;
                    continue;
                }
                let wait = retry_after(
                    response
                        .headers()
                        .get("Retry-After")
                        .and_then(|v| v.to_str().ok()),
                );
                let bytes = match response.bytes().await {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        if writing {
                            bail!("Resposta da gravação não recebida; resultado incerto. Retome a playlist para conferir antes de reenviar");
                        }
                        if attempt == 4 {
                            bail!("Falha de leitura da resposta Spotify após 5 tentativas");
                        }
                        self.backoff(attempt).await?;
                        continue;
                    }
                };
                let parsed = serde_json::from_slice::<Value>(&bytes);
                let body = parsed.as_ref().ok().cloned().unwrap_or(Value::Null);
                let quota = quota_exceeded(&body)
                    || String::from_utf8_lossy(&bytes)
                        .to_ascii_uppercase()
                        .contains("QUOTA_EXCEEDED");
                if quota || status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    let retry_at = Utc::now().checked_add_signed(
                        chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::days(1)),
                    );
                    let error:anyhow::Error=Paused { message: if quota {
                        "QUOTA_EXCEEDED: sincronização pausada; retome posteriormente com sync --resume".into()
                    } else { "HTTP 429: execução pausada; retome a operação após o prazo.".into() }, retry_at }.into();
                    return Err(if writing && !status.is_success() {
                        error.context(WriteRejected)
                    } else {
                        error
                    });
                }
                if status.is_success() {
                    return parsed.context("Resposta Spotify não contém JSON válido");
                }
                if !status.is_server_error() {
                    let error=anyhow::anyhow!("Spotify retornou HTTP {status}; verifique as permissões do aplicativo e execute login se necessário");
                    return Err(if writing {
                        error.context(WriteRejected)
                    } else {
                        error
                    });
                }
            }
            if writing {
                bail!("Gravação sem confirmação (rede/HTTP 5xx); resultado incerto. Retome a playlist para conferir antes de reenviar");
            }
            if attempt == 4 {
                bail!("Spotify indisponível após 5 tentativas; checkpoint preservado");
            }
            self.backoff(attempt).await?;
        }
        bail!("Limite de tentativas atingido; checkpoint preservado")
    }
}

#[async_trait]
impl Api for Spotify {
    async fn get(&mut self, path: &str, offset: Option<i64>) -> Result<Value> {
        let params = offset
            .map(|n| vec![("limit", "50".to_owned()), ("offset", n.to_string())])
            .unwrap_or_default();
        self.request(path, &params).await
    }
    async fn contains(&mut self, uris: &[String]) -> Result<Value> {
        if uris.is_empty() || uris.len() > 40 {
            bail!("Contains aceita entre 1 e 40 URIs por requisição");
        }
        self.request("/me/library/contains", &[("uris", uris.join(","))])
            .await
    }
    async fn followed_artists(&mut self, after: Option<&str>) -> Result<Value> {
        let mut params = vec![("type", "artist".to_owned()), ("limit", "50".to_owned())];
        if let Some(after) = after {
            params.push(("after", after.to_owned()));
        }
        self.request("/me/following", &params).await
    }
    async fn artist_albums(
        &mut self,
        artist: &str,
        offset: i64,
        groups: &str,
        market: Option<&str>,
    ) -> Result<Value> {
        if artist.is_empty() || !artist.bytes().all(|c| c.is_ascii_alphanumeric()) {
            bail!("ID de artista inválido");
        }
        let mut params = vec![
            ("limit", "10".to_owned()),
            ("offset", offset.to_string()),
            ("include_groups", groups.to_owned()),
        ];
        if let Some(market) = market {
            params.push(("market", market.to_owned()));
        }
        self.request(&format!("/artists/{artist}/albums"), &params)
            .await
    }
    async fn album_tracks(
        &mut self,
        album: &str,
        offset: i64,
        market: Option<&str>,
    ) -> Result<Value> {
        if album.is_empty() || !album.bytes().all(|c| c.is_ascii_alphanumeric()) {
            bail!("ID de álbum inválido");
        }
        let mut params = vec![("limit", "50".to_owned()), ("offset", offset.to_string())];
        if let Some(market) = market {
            params.push(("market", market.to_owned()));
        }
        self.request(&format!("/albums/{album}/tracks"), &params)
            .await
    }
    fn set_budget(&mut self, remaining: Option<u32>) {
        self.budget_remaining = remaining;
    }
    fn attempt_count(&self) -> u64 {
        self.attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    async fn server(
        replies: Vec<(&'static str, &'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            for (status, headers, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]).to_lowercase();
                assert!(request.contains("authorization: bearer test-token"));
                assert!(request.starts_with("get /me/tracks?limit=50&offset=0"));
                let reply=format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",body.len());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        (address, handle)
    }
    async fn client(base_url: String) -> Spotify {
        let mut client = Spotify::new(Auth::for_test(), 0).unwrap();
        client.base_url = base_url;
        client.http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        client
    }
    #[tokio::test]
    async fn actual_429_without_json_pauses_with_retry_after() {
        let (url, handle) = server(vec![(
            "429 Too Many Requests",
            "Retry-After: 120\r\n",
            "limit",
        )])
        .await;
        let error = client(url)
            .await
            .get("/me/tracks", Some(0))
            .await
            .unwrap_err();
        let pause = error.downcast_ref::<Paused>().unwrap();
        assert!(pause.retry_at.unwrap() > Utc::now() + chrono::Duration::seconds(118));
        handle.await.unwrap();
    }
    #[tokio::test]
    async fn actual_quota_403_stops_without_retrying() {
        let (url, handle) = server(vec![(
            "403 Forbidden",
            "",
            r#"{"error":{"reason":"QUOTA_EXCEEDED"}}"#,
        )])
        .await;
        let error = client(url)
            .await
            .get("/me/tracks", Some(0))
            .await
            .unwrap_err();
        assert!(error.is::<Paused>());
        assert!(error.to_string().contains("QUOTA_EXCEEDED"));
        handle.await.unwrap();
    }
    #[tokio::test]
    async fn transient_html_error_is_retried() {
        let (url, handle) = server(vec![
            ("503 Service Unavailable", "", "<html>Unavailable</html>"),
            ("200 OK", "", r#"{"items":[]}"#),
        ])
        .await;
        let value = client(url).await.get("/me/tracks", Some(0)).await.unwrap();
        assert_eq!(value, json_value());
        handle.await.unwrap();
    }
    #[tokio::test]
    async fn retry_attempts_cannot_exceed_request_budget() {
        let (url, handle) = server(vec![("503 Service Unavailable", "", "temporary")]).await;
        let mut api = client(url).await;
        api.set_budget(Some(1));
        let error = api.get("/me/tracks", Some(0)).await.unwrap_err();
        assert!(error.is::<BudgetExhausted>());
        assert_eq!(api.attempt_count(), 1);
        handle.await.unwrap();
    }
    #[tokio::test]
    async fn contains_uses_typed_encoded_query_without_sending_credentials_to_another_origin() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = vec![0; 8192];
            let n = socket.read(&mut bytes).await.unwrap();
            let request = String::from_utf8_lossy(&bytes[..n]);
            let target = request
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap();
            let parsed = url::Url::parse(&format!("http://local{target}")).unwrap();
            assert_eq!(parsed.path(), "/me/library/contains");
            assert_eq!(
                parsed.query_pairs().find(|(k, _)| k == "uris").unwrap().1,
                "spotify:track:abc,spotify:album:def"
            );
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n[true,false]").await.unwrap();
        });
        let mut api = client(url).await;
        assert_eq!(
            api.contains(&["spotify:track:abc".into(), "spotify:album:def".into()])
                .await
                .unwrap(),
            serde_json::json!([true, false])
        );
        handle.await.unwrap();
    }
    #[tokio::test]
    async fn discography_endpoints_use_correct_pagination_and_encoded_filters() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            for expected in [
                "/me/following",
                "/artists/Artist/albums",
                "/albums/Album/tracks",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = vec![0; 8192];
                let n = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..n]);
                assert!(request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-token"));
                let target = request
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let parsed = url::Url::parse(&format!("http://local{target}")).unwrap();
                assert_eq!(parsed.path(), expected);
                let query: std::collections::HashMap<_, _> = parsed
                    .query_pairs()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect();
                match expected {
                    "/me/following" => {
                        assert_eq!(query["limit"], "50");
                        assert_eq!(query["type"], "artist");
                        assert_eq!(query["after"], "LastArtist");
                    }
                    "/artists/Artist/albums" => {
                        assert_eq!(query["limit"], "10");
                        assert_eq!(query["offset"], "20");
                        assert_eq!(query["include_groups"], "album,single");
                        assert_eq!(query["market"], "BR");
                    }
                    _ => {
                        assert_eq!(query["limit"], "50");
                        assert_eq!(query["offset"], "50");
                        assert_eq!(query["market"], "BR");
                    }
                }
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
        });
        let mut api = client(base).await;
        api.followed_artists(Some("LastArtist")).await.unwrap();
        api.artist_albums("Artist", 20, "album,single", Some("BR"))
            .await
            .unwrap();
        api.album_tracks("Album", 50, Some("BR")).await.unwrap();
        handle.await.unwrap();
    }
    fn json_value() -> Value {
        serde_json::json!({"items":[]})
    }
    #[tokio::test]
    async fn server_redirect_is_not_followed() {
        let (url, handle) = server(vec![(
            "307 Temporary Redirect",
            "Location: http://127.0.0.1:1/\r\n",
            "{}",
        )])
        .await;
        let error = client(url)
            .await
            .get("/me/tracks", Some(0))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("307"));
        handle.await.unwrap();
    }
    async fn read_post_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let header_end;
        loop {
            let mut buf = [0; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buf[..n]);
            if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = i + 4;
                break;
            }
        }
        let header = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
        let length: usize = header
            .lines()
            .find_map(|l| l.strip_prefix("content-length: "))
            .unwrap()
            .parse()
            .unwrap();
        while data.len() < header_end + length {
            let mut buf = [0; 1024];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            data.extend_from_slice(&buf[..n]);
        }
        data
    }
    #[tokio::test]
    async fn playlist_posts_use_current_routes_json_and_never_repeat_uncertain_writes() {
        use crate::playlist::PlaylistApi;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            for (path, reply) in [
                ("/me/playlists", r#"{"id":"newPlaylist"}"#),
                (
                    "/playlists/newPlaylist/items",
                    r#"{"snapshot_id":"snapshot"}"#,
                ),
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let data = read_post_request(&mut socket).await;
                let header_end = data.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let header = String::from_utf8_lossy(&data[..header_end]).to_ascii_lowercase();
                assert!(header.starts_with(&format!("post {} ", path.to_ascii_lowercase())));
                assert!(header.contains("authorization: bearer test-token"));
                let length = data.len() - header_end;
                let body: Value =
                    serde_json::from_slice(&data[header_end..header_end + length]).unwrap();
                if path == "/me/playlists" {
                    assert_eq!(body["name"], "LSUA - All");
                    assert_eq!(body["public"], false);
                } else {
                    assert_eq!(body["uris"], serde_json::json!(["spotify:track:abc"]));
                }
                let response=format!("HTTP/1.1 201 Created\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",reply.len());
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut api = Spotify::new(Auth::for_test(), 0).unwrap();
        api.base_url = base;
        api.create_playlist("LSUA - All", false, "job")
            .await
            .unwrap();
        api.append_playlist("newPlaylist", &["spotify:track:abc".into()])
            .await
            .unwrap();
        task.await.unwrap();
        assert!(api
            .append_playlist("bad/id", &["spotify:track:abc".into()])
            .await
            .is_err());
        assert_eq!(api.attempt_count(), 2);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        api.base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = read_post_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        assert!(api
            .create_playlist("USSA - All", false, "job")
            .await
            .is_err());
        task.await.unwrap();
        assert_eq!(
            api.attempt_count(),
            3,
            "POST must never retry automatically after HTTP 5xx"
        );
    }
    #[tokio::test]
    async fn rejected_playlist_write_retains_quota_type_and_never_retries() {
        use crate::playlist::PlaylistApi;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut api = Spotify::new(Auth::for_test(), 0).unwrap();
        api.base_url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _request = read_post_request(&mut socket).await;
            socket.write_all(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 123\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let error = api
            .create_playlist("LSUA - All", false, "job")
            .await
            .unwrap_err();
        assert!(error.is::<Paused>());
        assert!(error.is::<WriteRejected>());
        assert_eq!(api.attempt_count(), 1);
        task.await.unwrap();
    }
}
