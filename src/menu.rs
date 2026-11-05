//! Line-oriented prompts work in Windows CMD without terminal escape sequences.
use super::*;
use std::io::{self, Write};

pub fn read(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    Ok(Some(line.trim().to_owned()))
}
pub fn confirm(prompt: &str) -> Result<bool> {
    Ok(read(&format!("{prompt} [s/N]: "))?
        .is_some_and(|s| s.eq_ignore_ascii_case("s") || s.eq_ignore_ascii_case("sim")))
}
fn playlist_options(kind: playlist::Kind) -> Result<Option<playlist::Options>> {
    println!("Datas: AAAA, MM/AAAA ou DD/MM/AAAA. Deixe vazias para incluir tudo.");
    let Some(from) = read("Período inicial (ou período único): ")? else {
        return Ok(None);
    };
    let Some(to) = read("Período final (opcional): ")? else {
        return Ok(None);
    };
    let Some(basis) = read("Filtrar por 1=adição à biblioteca, 2=lançamento do álbum [1]: ")?
    else {
        return Ok(None);
    };
    let date_basis = match basis.as_str() {
        "" | "1" => playlist::DateBasis::Added,
        "2" => playlist::DateBasis::Release,
        _ => bail!("Escolha 1 ou 2"),
    };
    if kind == playlist::Kind::Ussa && date_basis == playlist::DateBasis::Added {
        println!("Será usada a data em que o álbum foi salvo.");
    }
    let mut options = playlist::Options {
        kind,
        from: (!from.is_empty()).then_some(from),
        to: (!to.is_empty()).then_some(to),
        date_basis,
        name: None,
        public: false,
    };
    let default = options.default_name()?;
    let Some(name) = read(&format!("Nome [{default}]: "))? else {
        return Ok(None);
    };
    options.name = (!name.is_empty()).then_some(name);
    let Some(visibility) = read("Visibilidade: 1=privada, 2=pública [1]: ")? else {
        return Ok(None);
    };
    options.public = match visibility.as_str() {
        "" | "1" => false,
        "2" => true,
        _ => bail!("Escolha 1 ou 2"),
    };
    Ok(Some(options))
}
fn sync_command(resume: bool) -> Command {
    Command::Sync {
        full: false,
        reconcile: false,
        request_budget: None,
        album_budget: None,
        known_pages: None,
        resume_search_pages: None,
        resume,
        restart: false,
    }
}
pub async fn run(config: Config) -> Result<()> {
    loop {
        println!("\nSpotify Library Sync\nBanco: {}\n\n1. Login / autorizar biblioteca, artistas e playlists\n2. Sincronizar biblioteca\n3. Retomar sincronização da biblioteca\n4. Buscar discografias de artistas seguidos\n5. Retomar discografias\n6. Status da biblioteca e discografias\n7. Criar LSUA - Curtidas de álbuns não salvos\n8. Criar USSA - Não curtidas de álbuns salvos\n9. Retomar criação de playlist\n10. Status das playlists\n11. Estatísticas locais\n12. Listar artistas seguidos (offline)\n0. Sair",config.database.display());
        let Some(choice) = read("Opção: ")? else {
            break;
        };
        if choice == "0" {
            break;
        }
        let selected = (|| -> Result<Option<Command>> {
            Ok(Some(match choice.as_str() {
                "1" => Command::Login {
                    discography: true,
                    playlists: true,
                },
                "2" => sync_command(false),
                "3" => sync_command(true),
                "4" | "5" => Command::Discography {
                    command: DiscographyCommand::Sync {
                        options: discography::Options::default(),
                        resume: choice == "5",
                    },
                },
                "6" => Command::Status,
                "7" | "8" => {
                    let Some(options) = playlist_options(if choice == "7" {
                        playlist::Kind::Lsua
                    } else {
                        playlist::Kind::Ussa
                    })?
                    else {
                        return Ok(None);
                    };
                    Command::Playlist {
                        command: PlaylistCommand::Create {
                            options,
                            yes: false,
                            allow_incomplete: false,
                            request_budget: 1000,
                        },
                    }
                }
                "9" => {
                    let Some(job) = read("ID do job (Enter para o último pendente): ")? else {
                        return Ok(None);
                    };
                    Command::Playlist {
                        command: PlaylistCommand::Resume {
                            job: (!job.is_empty()).then_some(job),
                            request_budget: 1000,
                        },
                    }
                }
                "10" => Command::Playlist {
                    command: PlaylistCommand::Status,
                },
                "11" => Command::Stats,
                "12" => Command::Discography {
                    command: DiscographyCommand::Artists {
                        limit: 100,
                        offset: 0,
                    },
                },
                _ => {
                    println!("Opção inválida.");
                    return Ok(None);
                }
            }))
        })();
        match selected {
            Ok(Some(command)) => {
                if let Err(error) = execute(Cli {
                    config: config.clone(),
                    command: Some(command),
                })
                .await
                {
                    eprintln!("Erro: {error:#}");
                }
                if choice == "6" {
                    if let Err(error) = execute(Cli {
                        config: config.clone(),
                        command: Some(Command::Discography {
                            command: DiscographyCommand::Status,
                        }),
                    })
                    .await
                    {
                        eprintln!("Erro: {error:#}");
                    }
                }
            }
            Ok(None) => {}
            Err(error) => eprintln!("Erro: {error:#}"),
        }
    }
    Ok(())
}
