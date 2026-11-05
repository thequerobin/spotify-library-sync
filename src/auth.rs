use crate::config::Config;
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::{timeout, Duration},
};
use url::Url;

const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";

#[derive(Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
    #[serde(default)]
    pub scope: Option<String>,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
}

pub struct Auth {
    pub tokens: Tokens,
    client_id: String,
    http: reqwest::Client,
}

pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
fn entry(client: &str) -> Result<keyring::Entry> {
    keyring::Entry::new("spotify-library-sync", client)
        .context("Não foi possível acessar o cofre de credenciais do sistema")
}

impl Auth {
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            tokens: Tokens {
                access_token: "test-token".into(),
                refresh_token: "test-refresh".into(),
                expires_at: i64::MAX,
                scope: Some("user-library-read user-follow-read".into()),
            },
            client_id: "test-client".into(),
            http: reqwest::Client::new(),
        }
    }
    pub fn load(config: &Config) -> Result<Self> {
        let client_id = config.client_id()?.to_owned();
        let secret = entry(&client_id)?.get_password().context("Credenciais indisponíveis. Execute login; no Linux é necessário um Secret Service desbloqueado (GNOME Keyring/KWallet)")?;
        let tokens = serde_json::from_str(&secret)
            .context("Credenciais inválidas; execute login novamente")?;
        Ok(Self {
            tokens,
            client_id,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
        })
    }
    pub fn persist(&self) -> Result<()> {
        entry(&self.client_id)?
            .set_password(&serde_json::to_string(&self.tokens)?)
            .context("Falha ao salvar no cofre do sistema; nenhum token foi gravado em arquivo")
    }
    pub fn logout(config: &Config) -> Result<()> {
        match entry(config.client_id()?)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
    pub async fn refresh(&mut self) -> Result<()> {
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", self.tokens.refresh_token.as_str()),
                ("client_id", self.client_id.as_str()),
            ])
            .send()
            .await
            .context("Falha de rede ao renovar autenticação")?;
        if !response.status().is_success() {
            bail!(
                "Renovação rejeitada (HTTP {}). Execute login novamente",
                response.status()
            );
        }
        let token: TokenResponse = response
            .json()
            .await
            .context("Resposta de autenticação inválida")?;
        self.tokens.access_token = token.access_token;
        if let Some(refresh) = token.refresh_token {
            self.tokens.refresh_token = refresh;
        }
        if let Some(scope) = token.scope {
            self.tokens.scope = Some(scope);
        }
        self.tokens.expires_at = Utc::now().timestamp() + token.expires_in - 30;
        self.persist()
    }
    pub async fn access_token(&mut self) -> Result<&str> {
        if Utc::now().timestamp() >= self.tokens.expires_at {
            self.refresh().await?;
        }
        Ok(&self.tokens.access_token)
    }
    /// Opens the browser only for this explicit login command. Persistence happens after account validation.
    pub async fn login(config: &Config) -> Result<Self> {
        Self::login_with_discography(config, false).await
    }
    pub fn can_read_followed(&self) -> bool {
        self.tokens.scope.as_deref().is_some_and(|s| {
            s.split_whitespace()
                .any(|scope| scope == "user-follow-read")
        })
    }
    pub async fn login_with_discography(config: &Config, discography: bool) -> Result<Self> {
        let scopes = if discography {
            "user-library-read user-follow-read"
        } else {
            "user-library-read"
        };
        let client_id = config.client_id()?.to_owned();
        let redirect = config.redirect()?;
        let listener = TcpListener::bind(("127.0.0.1", redirect.port().unwrap()))
            .await
            .context("Não foi possível abrir a porta de callback")?;
        let verifier = random_string(96);
        let state = random_string(48);
        let mut authorize = Url::parse("https://accounts.spotify.com/authorize")?;
        authorize.query_pairs_mut().extend_pairs([
            ("client_id", client_id.as_str()),
            ("response_type", "code"),
            ("redirect_uri", config.redirect_uri.as_str()),
            ("scope", scopes),
            ("code_challenge_method", "S256"),
            ("code_challenge", challenge(&verifier).as_str()),
            ("state", state.as_str()),
        ]);
        println!("Autorize o aplicativo no navegador. Se ele não abrir, acesse:\n{authorize}");
        let _ = webbrowser::open(authorize.as_str());
        let code = timeout(Duration::from_secs(300), async {
            loop {
                let (mut socket, _) = listener.accept().await?;
                let mut request = Vec::new();
                let read = timeout(Duration::from_secs(5), async {
                    let mut chunk = [0u8; 1024];
                    loop {
                        let len = socket.read(&mut chunk).await?;
                        if len == 0 { break; }
                        request.extend_from_slice(&chunk[..len]);
                        if request.windows(4).any(|s| s == b"\r\n\r\n") || request.len() > 8192 { break; }
                    }
                    Ok::<_, std::io::Error>(())
                }).await;
                if !matches!(read, Ok(Ok(()))) { continue; }
                let line = String::from_utf8_lossy(&request);
                let parts: Vec<_> = line.lines().next().unwrap_or("").split_whitespace().collect();
                let parsed = parts.get(1).and_then(|path| Url::parse(&format!("http://127.0.0.1{path}")).ok());
                let valid = parsed.as_ref().is_some_and(|url| parts.first() == Some(&"GET")
                    && url.path() == redirect.path()
                    && url.query_pairs().filter(|(k,_)| k == "state").count() == 1
                    && url.query_pairs().any(|(k,v)| k == "state" && v == state));
                let (status, body) = if valid { ("200 OK", "Autorizacao recebida. Pode fechar esta janela.") }
                    else { ("400 Bad Request", "Callback invalido.") };
                let reply = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = timeout(Duration::from_secs(5), socket.write_all(reply.as_bytes())).await;
                if !valid { continue; }
                let url = parsed.unwrap();
                if url.query_pairs().any(|(k,_)| k == "error") { bail!("Autorização recusada pelo usuário"); }
                let code = url.query_pairs().find(|(k,_)| k == "code").map(|(_,v)| v.into_owned()).context("Callback sem código")?;
                return Ok::<_, anyhow::Error>(code);
            }
        }).await.context("Tempo de login esgotado (5 minutos)")??;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let response = http
            .post(TOKEN_URL)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", config.redirect_uri.as_str()),
                ("client_id", client_id.as_str()),
                ("code_verifier", verifier.as_str()),
            ])
            .send()
            .await
            .context("Falha de rede na autenticação")?;
        if !response.status().is_success() {
            bail!("Spotify recusou a autenticação: HTTP {}", response.status());
        }
        let token: TokenResponse = response
            .json()
            .await
            .context("Resposta de autenticação inválida")?;
        Ok(Self {
            tokens: Tokens {
                access_token: token.access_token,
                refresh_token: token
                    .refresh_token
                    .context("Spotify não retornou refresh token")?,
                expires_at: Utc::now().timestamp() + token.expires_in - 30,
                scope: Some(token.scope.unwrap_or_else(|| scopes.to_owned())),
            },
            client_id,
            http,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_tokens_load_without_new_scope_field() {
        let tokens: Tokens = serde_json::from_str(
            r#"{"access_token":"test","refresh_token":"test","expires_at":1}"#,
        )
        .unwrap();
        assert!(tokens.scope.is_none());
    }
    #[test]
    fn followed_permission_is_explicit_and_matches_a_complete_scope_name() {
        let mut auth = Auth::for_test();
        assert!(auth.can_read_followed());
        auth.tokens.scope = Some("user-library-read user-follow-read-extra".into());
        assert!(!auth.can_read_followed());
        auth.tokens.scope = None;
        assert!(!auth.can_read_followed());
    }
}
