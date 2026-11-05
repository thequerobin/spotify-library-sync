use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use fs2::FileExt;
use serde_json::{json, Value};
use spotify_library_sync::{
    auth::Auth,
    config::Config,
    database, discography, playlist,
    spotify::{Api, Spotify},
    sync,
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    QueryBuilder, Row, Sqlite,
};
use std::fs::OpenOptions;
mod menu;

#[derive(Parser)]
#[command(
    version,
    about = "Sincronize sua biblioteca Spotify para SQLite e consulte offline"
)]
struct Cli {
    #[command(flatten)]
    config: Config,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    /// Autenticar com PKCE; tokens no cofre do sistema.
    Login {
        /// Também autorizar a leitura dos artistas seguidos.
        #[arg(long)]
        discography: bool,
        /// Autorizar criação e retomada de playlists.
        #[arg(long)]
        playlists: bool,
    },
    /// Importar o catálogo dos artistas que você segue.
    Discography {
        #[command(subcommand)]
        command: DiscographyCommand,
    },
    /// Selecionar no SQLite e criar playlists no Spotify.
    Playlist {
        #[command(subcommand)]
        command: PlaylistCommand,
    },
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
#[derive(Subcommand)]
enum PlaylistCommand {
    /// Só após conferir no Spotify que uma criação incerta NÃO foi aplicada.
    RetryCreate {
        #[arg(long)]
        job: String,
        #[arg(long)]
        yes: bool,
        #[arg(long,default_value_t=1000,value_parser=clap::value_parser!(u32).range(5..=10000))]
        request_budget: u32,
    },
    /// Prévia inteiramente local, sem credenciais ou internet.
    Preview {
        #[command(flatten)]
        options: playlist::Options,
    },
    /// Mostrar a prévia e pedir confirmação antes de criar.
    Create {
        #[command(flatten)]
        options: playlist::Options,
        #[arg(long)]
        yes: bool,
        /// Aceitar uma base local incompleta ao usar --yes.
        #[arg(long)]
        allow_incomplete: bool,
        #[arg(long,default_value_t=1000,value_parser=clap::value_parser!(u32).range(5..=10000))]
        request_budget: u32,
    },
    /// Retomar a seleção congelada de um job, sem consultar novamente a biblioteca.
    Resume {
        #[arg(long)]
        job: Option<String>,
        #[arg(long,default_value_t=1000,value_parser=clap::value_parser!(u32).range(5..=10000))]
        request_budget: u32,
    },
    Status,
}
#[derive(Subcommand)]
enum DiscographyCommand {
    /// Sincronizar seguidos, descobrir lançamentos e importar as faixas pendentes.
    Sync {
        #[command(flatten)]
        options: discography::Options,
        #[arg(long)]
        resume: bool,
    },
    /// Importar apenas faixas pendentes usando a última lista confirmada de seguidos.
    Tracks {
        #[command(flatten)]
        options: discography::Options,
        #[arg(long)]
        resume: bool,
    },
    Status,
    /// Arquivar o job pendente, preservando o catálogo e a fila compartilhada de álbuns.
    Restart,
    Artists {
        #[arg(long,default_value_t=100,value_parser=clap::value_parser!(u32).range(1..=5000))]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    Albums {
        #[arg(long)]
        artist: String,
        #[arg(long,default_value_t=100,value_parser=clap::value_parser!(u32).range(1..=5000))]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
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
    let command = cli.command.context("Escolha um comando")?;
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
    let disc_mutates = matches!(
        &command,
        Command::Discography {
            command: DiscographyCommand::Sync { .. }
                | DiscographyCommand::Tracks { .. }
                | DiscographyCommand::Restart
        }
    );
    let playlist_mutates = matches!(
        &command,
        Command::Playlist {
            command: PlaylistCommand::Create { .. }
                | PlaylistCommand::Resume { .. }
                | PlaylistCommand::RetryCreate { .. }
        }
    );
    if playlist_mutates
        || disc_mutates
        || matches!(
            &command,
            Command::Sync { .. }
                | Command::CheckSaved { .. }
                | Command::Login { .. }
                | Command::Logout
        )
    {
        lock.try_lock_exclusive()
            .context("Outra instância está usando este banco")?;
    }
    let mut db = database::open(&absolute).await?;
    match command {
        Command::Login {
            discography,
            playlists,
        } => {
            let auth = Auth::login_with_options(&cli.config, discography, playlists).await?;
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
        Command::Playlist { command } => {
            match command {
                PlaylistCommand::RetryCreate {
                    job,
                    yes,
                    request_budget,
                } => {
                    let row=sqlx::query("SELECT public FROM playlist_job WHERE id=? AND state='creating' AND playlist_id IS NULL").bind(&job).fetch_optional(&mut db).await?.context("Job não está em criação sem confirmação")?;
                    println!("Confira no Spotify: esta opção pode criar uma segunda playlist se a primeira já existir.");
                    if !yes && !menu::confirm("Confirma que a playlist anterior NÃO foi criada e deseja tentar novamente?")? {return Ok(());}
                    let auth = Auth::load(&cli.config)?;
                    if !auth.can_create_playlists(row.get::<i64, _>("public") == 1) {
                        bail!("Execute login --discography --playlists");
                    }
                    sync::check_cooldown(&mut db).await?;
                    sqlx::query("UPDATE playlist_job SET state='ready',last_error=NULL,updated_at=? WHERE id=?").bind(database::now()).bind(&job).execute(&mut db).await?;
                    run_playlist(&mut db, &cli.config, auth, &job, request_budget).await?;
                }

                PlaylistCommand::Preview { options } => {
                    let preview = playlist::preview(&mut db, &options).await?;
                    print_json(&serde_json::to_value(preview)?)?;
                }
                PlaylistCommand::Status => print_json(&playlist::status(&mut db).await?)?,
                PlaylistCommand::Create {
                    options,
                    yes,
                    allow_incomplete,
                    request_budget,
                } => {
                    let preview = playlist::preview(&mut db, &options).await?;
                    println!(
                        "\nNome: {}\nVisibilidade: {}\nMúsicas: {}",
                        preview.name,
                        if preview.public {
                            "Pública"
                        } else {
                            "Privada"
                        },
                        preview.count
                    );
                    for warning in &preview.warnings {
                        println!("Aviso: {warning}");
                    }
                    for track in preview.candidates.iter().take(10) {
                        println!("  {}", track.name.as_deref().unwrap_or(&track.id));
                    }
                    if preview.count == 0 {
                        println!("Nenhuma música encontrada. Nenhuma playlist criada.");
                        return Ok(());
                    }
                    if yes && preview.incomplete && !allow_incomplete {
                        bail!("Base incompleta. Confira os avisos e use --allow-incomplete para aceitar a seleção local parcial");
                    }
                    if !yes
                        && !menu::confirm(
                            "Criar esta playlist no Spotify usando somente estes dados locais?",
                        )?
                    {
                        println!("Criação cancelada.");
                        return Ok(());
                    }
                    let auth = Auth::load(&cli.config)?;
                    if !auth.can_create_playlists(preview.public) {
                        bail!(
                            "Autorize playlists com spotify-sync login --discography --playlists"
                        );
                    }
                    sync::check_cooldown(&mut db).await?;
                    let job = playlist::freeze(&mut db, &options, &preview).await?;
                    println!("Job salvo: {job}");
                    run_playlist(&mut db, &cli.config, auth, &job, request_budget).await?;
                }
                PlaylistCommand::Resume {
                    job,
                    request_budget,
                } => {
                    let job=match job {Some(id)=>id,None=>sqlx::query_scalar::<_,String>("SELECT id FROM playlist_job WHERE state<>'completed' ORDER BY created_at DESC,id DESC LIMIT 1").fetch_optional(&mut db).await?.context("Nenhuma playlist pendente")?};
                    let public: i64 =
                        sqlx::query_scalar("SELECT public FROM playlist_job WHERE id=?")
                            .bind(&job)
                            .fetch_optional(&mut db)
                            .await?
                            .context("Job não encontrado")?;
                    let auth = Auth::load(&cli.config)?;
                    if !auth.can_create_playlists(public == 1) {
                        bail!(
                            "Autorize playlists com spotify-sync login --discography --playlists"
                        );
                    }
                    run_playlist(&mut db, &cli.config, auth, &job, request_budget).await?;
                }
            }
        }
        Command::Discography { command } => match command {
            DiscographyCommand::Sync { options, resume } => {
                run_discography(&mut db, &cli.config, options, resume, "sync").await?
            }
            DiscographyCommand::Tracks { options, resume } => {
                run_discography(&mut db, &cli.config, options, resume, "tracks").await?
            }
            DiscographyCommand::Status => print_json(&discography::status(&mut db).await?)?,
            DiscographyCommand::Restart => {
                discography::restart(&mut db).await?;
                println!("Job arquivado; catálogo e álbuns pendentes preservados.");
            }
            DiscographyCommand::Artists { limit, offset } => {
                print_json(&discography::artists(&mut db, limit, offset).await?)?
            }
            DiscographyCommand::Albums {
                artist,
                limit,
                offset,
            } => print_json(&discography::albums(&mut db, &artist, limit, offset).await?)?,
        },
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
async fn run_playlist(
    db: &mut sqlx::SqliteConnection,
    config: &Config,
    auth: Auth,
    job: &str,
    request_budget: u32,
) -> Result<()> {
    sync::check_cooldown(db).await?;
    let mut api = Spotify::new(auth, config.request_interval_ms)?;
    api.set_budget(Some(request_budget));
    let result = tokio::select! {
        result=playlist::run(db,&mut api,job)=>Some(result),
        signal=tokio::signal::ctrl_c()=>{signal?;None},
    };
    match result {
        Some(Ok(summary)) => print_json(&summary)?,
        Some(Err(error)) => {
            playlist::record_error(db, job, &error).await?;
            eprintln!("Continue com spotify-sync playlist resume --job {job}");
            return Err(error);
        }
        None => println!("Playlist pausada. Continue com spotify-sync playlist resume --job {job}"),
    }
    Ok(())
}
async fn run_discography(
    db: &mut sqlx::SqliteConnection,
    config: &Config,
    options: discography::Options,
    resume: bool,
    kind: &str,
) -> Result<()> {
    sync::check_cooldown(db).await?;
    let auth = Auth::load(config)?;
    if kind == "sync" && !auth.can_read_followed() {
        bail!("Autorize a leitura de artistas seguidos com spotify-sync login --discography; depois repita este comando");
    }
    let job = discography::prepare(db, resume, kind, options).await?;
    let mut api = Spotify::new(auth, config.request_interval_ms)?;
    let result = tokio::select! {
        result=discography::run(db,&mut api,&job)=>Some(result),
        signal=tokio::signal::ctrl_c()=>{signal?;None},
    };
    match result {
        Some(Ok(summary)) => print_json(&summary)?,
        Some(Err(error)) => {
            discography::record_error(db, &job, &error).await?;
            return Err(error);
        }
        None => {
            sqlx::query("UPDATE discografia_job SET status='paused',pause_reason='user',updated_at=? WHERE id=?").bind(database::now()).bind(&job).execute(&mut *db).await?;
            println!("Discografia pausada. Continue com discography sync --resume.");
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
    let cli = Cli::parse();
    let result = if cli.command.is_none() {
        menu::run(cli.config).await
    } else {
        execute(cli).await
    };
    if let Err(error) = result {
        eprintln!("Erro: {error:#}");
        std::process::exit(1);
    }
}
