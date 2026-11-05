use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
use serde_json::{json, Value};
use spotify_library_sync::{
    auth::Auth,
    config::Config,
    database,
    spotify::{Api, Spotify},
    sync,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    QueryBuilder, Row, Sqlite,
};
use std::fs::OpenOptions;

#[derive(Parser)]
#[command(
    version,
    about = "Sincronize sua biblioteca Spotify para SQLite e consulte offline"
)]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Autenticar com PKCE; tokens no cofre do sistema.
    Login,
    /// Remover credenciais locais.
    Logout,
    /// Sincronizar sem modificar a biblioteca no Spotify.
    Sync {
        #[arg(long, conflicts_with_all = ["resume", "reconcile"])]
        full: bool,
        #[arg(long, conflicts_with = "resume")]
        reconcile: bool,
        /// Limite incremental de tentativas HTTP por coleção nesta execução.
        #[arg(long, env="SPOTIFY_COLLECTION_BUDGET", value_parser=clap::value_parser!(u32).range(5..))]
        request_budget: Option<u32>,
        /// Limite incremental de tentativas HTTP de faixas complementares nesta execução.
        #[arg(long, env="SPOTIFY_ALBUM_BUDGET", value_parser=clap::value_parser!(u32).range(1..))]
        album_budget: Option<u32>,
        #[arg(long, value_parser=clap::value_parser!(u32).range(1..=10))]
        known_pages: Option<u32>,
        /// Limite de chamadas para reencontrar a referência numa importação completa.
        #[arg(long, value_parser=clap::value_parser!(u32).range(1..=100))]
        resume_search_pages: Option<u32>,
        #[arg(long, conflicts_with = "restart")]
        resume: bool,
        #[arg(long)]
        restart: bool,
    },
    /// Confirmar estados somente das URIs selecionadas (máximo 40 por lote).
    CheckSaved {
        #[arg(long="uri", required=true, num_args=1..)]
        uris: Vec<String>,
    },
    Status,
    Stats,
    Tracks {
        #[arg(long)]
        liked: bool,
        #[arg(long, requires = "liked")]
        album_not_saved: bool,
        /// Lançamento: AAAA, AAAA-MM ou AAAA-MM-DD.
        #[arg(long)]
        release: Option<String>,
        /// ID Spotify ou nome exato do artista.
        #[arg(long)]
        artist: Option<String>,
        #[command(flatten)]
        filters: Filters,
    },
    Albums {
        #[arg(long)]
        saved: bool,
        #[arg(long)]
        with_unliked_tracks: bool,
        #[arg(long)]
        release: Option<String>,
        #[arg(long)]
        artist: Option<String>,
        #[command(flatten)]
        filters: Filters,
    },
}
#[derive(Args)]
struct Filters {
    /// Início da data em que foi salvo (AAAA, AAAA-MM ou AAAA-MM-DD).
    #[arg(long)]
    from: Option<String>,
    /// Final inclusivo da data em que foi salvo.
    #[arg(long)]
    to: Option<String>,
    #[arg(long, default_value_t=100, value_parser=clap::value_parser!(u32).range(1..=5000))]
    limit: u32,
    #[arg(long, default_value_t = 0)]
    offset: u32,
}
fn date_bounds(text: &str) -> Result<(String, String)> {
    use chrono::{Datelike, NaiveDate};
    let date = match text.len() {
        4 => NaiveDate::parse_from_str(&format!("{text}-01-01"), "%Y-%m-%d")?,
        7 => NaiveDate::parse_from_str(&format!("{text}-01"), "%Y-%m-%d")?,
        10 => NaiveDate::parse_from_str(text, "%Y-%m-%d")?,
        _ => bail!("Data inválida: use AAAA, AAAA-MM ou AAAA-MM-DD"),
    };
    if date.year() < 1 || date.year() > 9998 {
        bail!("Ano deve estar entre 0001 e 9998");
    }
    let end = match text.len() {
        4 => NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap(),
        7 => {
            if date.month() == 12 {
                NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap()
            } else {
                NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).unwrap()
            }
        }
        _ => date.succ_opt().context("Data fora do intervalo")?,
    };
    Ok((
        date.format("%Y-%m-%d").to_string(),
        end.format("%Y-%m-%d").to_string(),
    ))
}
fn print_json(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

async fn execute(cli: Cli) -> Result<()> {
    if let Some(parent) = cli
        .config
        .database
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let absolute = if cli.config.database.exists() {
        cli.config.database.canonicalize()?
    } else {
        let filename = cli
            .config
            .database
            .file_name()
            .context("Caminho do banco inválido")?;
        cli.config
            .database
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(std::path::Path::new("."))
            .canonicalize()?
            .join(filename)
    };
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(absolute.with_extension("lockfile"))?;
    if matches!(
        &cli.command,
        Command::Sync { .. } | Command::CheckSaved { .. } | Command::Login | Command::Logout
    ) {
        lock.try_lock_exclusive()
            .context("Outra instância está usando este banco")?;
    }
    let mut db = database::open(&absolute).await?;
    match cli.command {
        Command::Login => {
            let auth = Auth::login(&cli.config).await?;
            let mut api = Spotify::new(auth, cli.config.request_interval_ms)?;
            let profile = match api.get("/me", None).await {
                Ok(profile) => profile,
                Err(error) => {
                    sync::remember_cooldown(&mut db, &error).await?;
                    return Err(error);
                }
            };
            let id = profile["id"]
                .as_str()
                .context("Spotify não retornou ID da conta")?;
            database::bind_account(&mut db, id).await?;
            api.auth.persist()?;
            println!("Autenticação concluída para {id}. Execute spotify-sync sync.");
        }
        Command::Logout => {
            Auth::logout(&cli.config)?;
            println!("Credenciais locais removidas.");
        }
        Command::Sync {
            full,
            reconcile,
            request_budget,
            album_budget,
            known_pages,
            resume_search_pages,
            resume,
            restart,
        } => {
            let auth = Auth::load(&cli.config)?;
            let mut api = Spotify::new(auth, cli.config.request_interval_ms)?;
            // Honor persisted cooldown before making even a profile request.
            sync::check_cooldown(&mut db).await?;
            if restart {
                sync::restart(&mut db).await?;
            }
            let options = sync::Options {
                full,
                reconcile,
                collection_budget: request_budget,
                album_budget,
                known_pages,
                resume_search_pages,
            };
            let job = sync::prepare_sync(&mut db, resume, &options).await?;
            let mode: String = sqlx::query_scalar("SELECT sync_mode FROM sync_job WHERE id=?")
                .bind(&job)
                .fetch_one(&mut db)
                .await?;
            eprintln!(
                "Modo: {mode}. {}",
                if mode == "incremental" {
                    "Cobertura: janela recente; remoções antigas não verificadas."
                } else {
                    "Listagem completa solicitada ou importação inicial/legada."
                }
            );
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(&absolute)
                        .foreign_keys(true)
                        .busy_timeout(std::time::Duration::from_secs(10)),
                )
                .await?;
            api.metrics = Some((pool.clone(), job.clone()));
            let profile_check = async {
                let profile = match api.get("/me", None).await {
                    Ok(profile) => profile,
                    Err(error) => {
                        sync::remember_cooldown(&mut db, &error).await?;
                        return Err(error);
                    }
                };
                database::bind_account(&mut db, profile["id"].as_str().context("Perfil sem ID")?)
                    .await
            }
            .await;
            if let Err(error) = profile_check {
                sync::record_error(&mut db, &job, &error).await?;
                pool.close().await;
                return Err(error);
            }
            let result = tokio::select! {
                result = sync::run(&mut db,&mut api,&job,cli.config.metadata_ttl_days) => Some(result),
                signal = tokio::signal::ctrl_c() => {signal?; None},
            };
            match result {
                Some(Ok(summary)) => {
                    print_json(&summary)?;
                    print_json(&database::stats(&mut db).await?)?;
                }
                Some(Err(error)) => {
                    sync::record_error(&mut db, &job, &error).await?;
                    pool.close().await;
                    return Err(error);
                }
                None => {
                    sqlx::query("UPDATE sync_job SET status='paused',updated_at=? WHERE id=?")
                        .bind(database::now())
                        .bind(job)
                        .execute(&mut db)
                        .await?;
                    println!("Execução pausada. Continue com spotify-sync sync --resume.");
                }
            }
            pool.close().await;
        }
        Command::Stats => {
            sync::warn_coverage(&mut db).await?;
            print_json(&database::stats(&mut db).await?)?;
        }
        Command::CheckSaved { uris } => {
            sync::check_cooldown(&mut db).await?;
            let auth = Auth::load(&cli.config)?;
            let mut api = Spotify::new(auth, cli.config.request_interval_ms)?;
            let profile = match api.get("/me", None).await {
                Ok(profile) => profile,
                Err(error) => {
                    sync::remember_cooldown(&mut db, &error).await?;
                    return Err(error);
                }
            };
            database::bind_account(&mut db, profile["id"].as_str().context("Perfil sem ID")?)
                .await?;
            print_json(&sync::check_saved(&mut db, &mut api, &uris).await?)?;
        }
        Command::Status => print_json(&sync::status(&mut db).await?)?,
        Command::Tracks {
            liked,
            album_not_saved,
            release,
            artist,
            filters,
        } => {
            sync::warn_coverage(&mut db).await?;
            let mut q = QueryBuilder::<Sqlite>::new("SELECT json_object('id',m.id,'name',m.name,'album_id',m.album_id,'album',a.name,'release_date',a.release_date,'is_saved',json(CASE WHEN m.is_saved=1 THEN 'true' ELSE 'false' END),'saved_at',m.saved_at,'spotify_url',m.spotify_url) AS item FROM musica m LEFT JOIN album a ON a.id=m.album_id WHERE 1=1");
            if liked {
                q.push(" AND m.is_saved=1");
            }
            if album_not_saved {
                q.push(" AND a.is_saved=0");
            }
            if let Some(release) = release {
                date_bounds(&release)?;
                q.push(" AND substr(a.release_date,1,")
                    .push_bind(release.len() as i64)
                    .push(")=")
                    .push_bind(release);
            }
            if let Some(artist) = artist {
                q.push(" AND EXISTS(SELECT 1 FROM musica_artista ma JOIN artista ar ON ar.id=ma.artista_id WHERE ma.musica_id=m.id AND (ar.id=").push_bind(artist.clone()).push(" OR ar.name=").push_bind(artist).push(" COLLATE NOCASE))");
            }
            query_filters(&mut q, "m", filters)?;
            let rows = q.build().fetch_all(&mut db).await?;
            let values: Result<Vec<Value>, _> = rows
                .iter()
                .map(|r| serde_json::from_str(r.get("item")))
                .collect();
            print_json(&json!(values?))?;
        }
        Command::Albums {
            saved,
            with_unliked_tracks,
            release,
            artist,
            filters,
        } => {
            sync::warn_coverage(&mut db).await?;
            let mut q = QueryBuilder::<Sqlite>::new("SELECT json_object('id',a.id,'name',a.name,'release_date',a.release_date,'release_date_precision',a.release_date_precision,'is_saved',json(CASE WHEN a.is_saved=1 THEN 'true' ELSE 'false' END),'saved_at',a.saved_at,'spotify_url',a.spotify_url) AS item FROM album a WHERE 1=1");
            if saved || with_unliked_tracks {
                q.push(" AND a.is_saved=1");
            }
            if with_unliked_tracks {
                q.push(" AND EXISTS(SELECT 1 FROM album_musica am JOIN musica m ON m.id=am.musica_id WHERE am.album_id=a.id AND m.is_saved=0)");
            }
            if let Some(release) = release {
                date_bounds(&release)?;
                q.push(" AND substr(a.release_date,1,")
                    .push_bind(release.len() as i64)
                    .push(")=")
                    .push_bind(release);
            }
            if let Some(artist) = artist {
                q.push(" AND EXISTS(SELECT 1 FROM album_artista aa JOIN artista ar ON ar.id=aa.artista_id WHERE aa.album_id=a.id AND (ar.id=").push_bind(artist.clone()).push(" OR ar.name=").push_bind(artist).push(" COLLATE NOCASE))");
            }
            query_filters(&mut q, "a", filters)?;
            let rows = q.build().fetch_all(&mut db).await?;
            let values: Result<Vec<Value>, _> = rows
                .iter()
                .map(|r| serde_json::from_str(r.get("item")))
                .collect();
            print_json(&json!(values?))?;
        }
    }
    Ok(())
}
fn query_filters<'a>(q: &mut QueryBuilder<'a, Sqlite>, alias: &str, f: Filters) -> Result<()> {
    let from = f.from.as_deref().map(date_bounds).transpose()?.map(|b| b.0);
    let to = f.to.as_deref().map(date_bounds).transpose()?.map(|b| b.1);
    if from.as_ref().zip(to.as_ref()).is_some_and(|(f, t)| f >= t) {
        bail!("Data inicial posterior à data final");
    }
    if let Some(from) = from {
        q.push(format!(" AND {alias}.saved_at>=")).push_bind(from);
    }
    if let Some(to) = to {
        q.push(format!(" AND {alias}.saved_at<")).push_bind(to);
    }
    q.push(format!(" ORDER BY {alias}.name,{alias}.id LIMIT "))
        .push_bind(f.limit as i64)
        .push(" OFFSET ")
        .push_bind(f.offset as i64);
    Ok(())
}
#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = execute(Cli::parse()).await {
        eprintln!("Erro: {error:#}");
        std::process::exit(1);
    }
}
