# Spotify Library Sync

CLI local em Rust para manter músicas curtidas, álbuns salvos e suas faixas em SQLite. Após a primeira importação, o comando normal usa uma busca incremental econômica. Os dados podem ser consultados offline.

## Instalação e configuração

Use o executável para seu sistema nos artefatos do [GitHub Actions](https://github.com/thequerobin/spotify-library-sync/actions), ou compile com Rust estável:

```bash
cargo build --release --locked
```

No Windows, o executável fica em `target\release\spotify-sync.exe`. No Linux/macOS, em `target/release/spotify-sync`. Para compilar no Windows, use Rust/MSVC com os Build Tools de C++. No Linux, tenha compilador C e `pkg-config`; SQLite e a dependência D-Bus são compilados pelas dependências do projeto.

1. Crie um aplicativo no [Spotify Developer Dashboard](https://developer.spotify.com/dashboard).
2. Cadastre o Redirect URI `http://127.0.0.1:8888/callback`.
3. Copie `.env.example` para `.env` e preencha `SPOTIFY_CLIENT_ID`. Não é necessário Client Secret.
4. Abra um terminal na pasta do programa e execute `spotify-sync login`, autorizando no navegador.
5. Execute `spotify-sync sync` para a primeira importação.

Se a porta estiver ocupada, escolha outra, como `8899`, tanto no `.env` quanto no Dashboard. A URI deve coincidir. O aplicativo solicita somente `user-library-read` e usa PKCE com validação de `state`.

Os tokens ficam no cofre de credenciais do sistema, sem fallback para texto puro. No Linux, é necessário um Secret Service desbloqueado, como GNOME Keyring ou KWallet, e uma sessão D-Bus. O aplicativo usa uma conta por banco e confere o ID de `/me` antes de alterar seus dados.

PowerShell, executando a partir do código-fonte:

```powershell
Copy-Item .env.example .env
# Preencha o Client ID em .env.
cargo build --release --locked
.\target\release\spotify-sync.exe login
.\target\release\spotify-sync.exe sync
```

## Atualizar uma instalação v0.1.0

Substitua o executável e mantenha seu `.env` e a pasta `data`. As migrations são aplicadas automaticamente; não é necessário apagar o banco ou refazer o login.

- Se a importação anterior terminou, o próximo `sync` será incremental.
- Se há um job v0.1.0 pendente, use `sync --resume`. Ele termina usando o protocolo anterior; o modo aparece no terminal como `legacy`.
- O programa não deduz uma importação completa apenas pela quantidade de registros ou pela data mais recente. Se houver estados confirmados sem histórico suficiente, ele pede `--reconcile`, sem iniciar uma varredura cara silenciosamente.

## Modos de sincronização

| Comando | Comportamento |
|---|---|
| `spotify-sync sync` | Importação inicial quando necessário; depois, busca incremental nas páginas recentes. |
| `spotify-sync sync --resume` | Retoma o job existente, com seu modo e os limites persistidos. |
| `spotify-sync sync --reconcile` | Releitura completa para confirmar adições e remoções; reutiliza faixas já importadas. |
| `spotify-sync sync --full` | Reconciliação completa e reimportação das faixas dos álbuns salvos. |
| `spotify-sync sync --restart` | Arquiva o job incompleto, preserva catálogo, estados publicados e faixas pendentes, e inicia outra tentativa. |
| `spotify-sync status` | Mostra modo, cobertura, etapas, limites, checkpoints e último erro. |

Reconciliação completa nunca é iniciada automaticamente por TTL vencido, redução do total, ausência de âncora ou alteração da biblioteca.

### Como funciona o incremental

O programa congela uma referência dos IDs e datas já confirmados no SQLite, consulta páginas de até 50 itens e para após duas páginas consecutivas inteiramente conhecidas, ou no fim da coleção. Itens cadastrados no catálogo, mas não confirmados como curtidos/salvos, não provocam essa parada. Datas iguais são comparadas juntamente com os IDs.

Não há segunda passagem integral no modo incremental. Quando necessário, há uma conferência adicional da primeira página. Numa base estável de 21.948 curtidas e mais de duas páginas de álbuns, sem novidades, retries ou faixas pendentes, a meta validada é **5 chamadas de dados**: uma de perfil, duas de curtidas e duas de álbuns. Cinco curtidas novas no início normalmente exigem até 7 chamadas contando as duas coleções e o perfil.

A API não documenta um filtro `since` nem garante uma ordenação temporal nesses endpoints. A parada antecipada é uma heurística sobre a ordem observada. **A cobertura é da janela recente:** alterações antigas fora dela podem não aparecer. O programa não anuncia sincronização exata de toda a biblioteca nesse modo. Uma ordem incompatível pausa a coleta com cobertura inconclusiva; não dispara um full automático.

### Estados salvos, remoções e consultas

Uma música curtida é independente de seu álbum estar salvo. Músicas vindas de álbuns não são automaticamente curtidas. O catálogo conserva itens removidos, com o estado salvo atualizado somente após confirmação.

No incremental, somente observações positivas são publicadas, por coleção concluída. Nunca se marca como removido um item apenas porque não apareceu nas páginas recentes. Uma falha posterior nas faixas não desfaz uma coleção já publicada. O status informa quando ocorreu a última reconciliação completa; as consultas avisam em stderr quando há cobertura recente ou reconciliação antiga, preservando o JSON em stdout.

Para confirmar itens específicos, sem percorrer a biblioteca inteira:

```bash
spotify-sync check-saved --uri spotify:track:ID1 spotify:album:ID2
```

Esse comando deduplica as URIs e usa lotes de até 40. Somente itens já existentes no catálogo são atualizados; URIs desconhecidas apenas têm seu resultado exibido. Um falso confirmado altera `is_saved` sem apagar metadados. O endpoint não informa a data de adição: o resultado indica `saved_at_verified=false` e conserva a última data conhecida, sem inventar outra.

Erros ou respostas incompletas não significam remoção. Reconciliar todos os IDs via `check-saved` também seria caro; ele é destinado a seleções explícitas.

## Limites, progresso e retomada

Por padrão, cada invocação incremental permite até **20 tentativas HTTP por coleção** e **20 para a fila de faixas**. As tentativas repetidas também consomem orçamento. O perfil é separado; chamadas OAuth não entram nessa contagem de dados.

```bash
spotify-sync sync --request-budget 40 --album-budget 10
spotify-sync sync --resume --request-budget 40
spotify-sync sync --known-pages 3
```

O orçamento de coleção deve ser pelo menos 5 para permitir revalidação e avanço na retomada. `--known-pages` aceita 1 a 10; o padrão é 2. Esses limites valem para o modo incremental; importação inicial e comandos completos explicitamente solicitados percorrem as coleções por inteiro.

Ao esgotar o orçamento, a execução pausa com `request_budget_exhausted`. Lotes e checkpoints ficam gravados na mesma transação; a fronteira incompleta não avança. `sync --resume` revalida o cabeçalho e até duas páginas próximas ao checkpoint. Se as âncoras coincidirem, continua adiante; se divergirem, reposiciona a janela recente, mantendo os metadados e a referência anterior. Essa conferência também é heurística, não um snapshot da biblioteca.

A fila de álbuns reaproveita as faixas embutidas nas respostas e consulta somente páginas complementares. No incremental, álbuns completos não são recarregados só porque seu TTL venceu. Importações incompletas e quantidades de faixas alteradas continuam pendentes. Uma mudança de estado salvo não torna incompleta uma importação de faixas já válida.

Todas as chamadas passam por um cliente serial, com intervalo padrão de 500 ms. Falhas de conexão e respostas 5xx têm até cinco tentativas, com backoff e jitter, respeitando também o orçamento disponível. Um 401 permite renovar o token.

`429` e `QUOTA_EXCEEDED` interrompem a execução imediatamente e persistem o cooldown. `Retry-After` aceita segundos ou HTTP-date; sem header, o padrão é 60 segundos. O prazo é conferido antes de chamadas de dados e também antes de `--restart`; passar o prazo não garante que a quota já foi liberada.

Ctrl+C pausa a execução. Se fechar o terminal ou reiniciar o PC, os lotes confirmados permanecem; execute `sync --resume` depois. O aplicativo não permanece rodando quando fechado nem inicia automaticamente com o Windows.

## Consultas offline

```bash
spotify-sync stats
spotify-sync tracks --liked
spotify-sync tracks --liked --album-not-saved
spotify-sync albums --saved
spotify-sync albums --with-unliked-tracks
spotify-sync tracks --artist "Nome do artista"
spotify-sync tracks --release 2026
spotify-sync tracks --release 2026-02
spotify-sync tracks --liked --from 2026-01 --to 2026-02
spotify-sync albums --saved --from 2026 --to 2026
spotify-sync tracks --limit 100 --offset 100
spotify-sync logout
```

Consultas locais não exigem Client ID, tokens ou internet. Listagens retornam JSON, com 100 itens por padrão, até 5.000 por chamada e paginação por offset. `--artist` aceita ID ou nome exato com comparação SQLite `NOCASE` (sem normalização de acentos). Datas de salvamento usam UTC e final inclusivo; filtros aceitam ano, ano/mês ou data completa. `--release` compara o prefixo da data original, respeitando a precisão informada pelo Spotify.

O banco padrão é `./data/spotify_library.db`, relativo à pasta em que o comando é executado. Use sempre a mesma pasta ou `--database` com caminho absoluto. SQLite usa WAL, chaves estrangeiras, migrations e índices. Um lock impede sincronizações simultâneas no mesmo banco; `status` e consultas continuam disponíveis.

## Configuração

| Variável | Padrão | Finalidade |
|---|---|---|
| `SPOTIFY_CLIENT_ID` | obrigatório para login/sync | Client ID do aplicativo. |
| `SPOTIFY_REDIRECT_URI` | `http://127.0.0.1:8888/callback` | Callback cadastrado no Dashboard. |
| `SPOTIFY_DATABASE` | `./data/spotify_library.db` | Arquivo SQLite. |
| `SPOTIFY_REQUEST_INTERVAL_MS` | `500` | Intervalo mínimo entre chamadas. |
| `SPOTIFY_METADATA_TTL_DAYS` | `30` | Compatibilidade com jobs legados; o incremental não faz atualização ampla por TTL. |
| `SPOTIFY_COLLECTION_BUDGET` | `20` em novos jobs | Limite incremental por coleção e invocação. |
| `SPOTIFY_ALBUM_BUDGET` | `20` em novos jobs | Limite incremental da fila de faixas e invocação. |
| `RUST_LOG` | `warn` | Nível de logs. |

Flags explícitas de orçamento podem aumentar ou reduzir o limite numa retomada. Sem sobrescrita, o limite do job é mantido. Nunca versione `.env`, tokens, banco, cache ou a pasta `target`.

## Código e validação

- `src/sync.rs`: modo incremental, referências congeladas, publicação por coleção, orçamentos, status e confirmação direcionada.
- `src/legacy_sync.rs`: listagens completas e retomada do protocolo v0.1.0.
- `src/spotify.rs`: transporte, paginação, query tipada, quotas e retries.
- `src/database.rs`: catálogo, JSON original, vínculos e reconciliação completa protegida contra uso incremental.
- `src/auth.rs`: PKCE, callback, refresh e cofre do sistema.
- `src/main.rs`: CLI e consultas offline.
- `migrations/`: schema compatível, sem recriar o banco.
- [Spec incremental](docs/incremental-spec.md): contrato e critérios de aceite.
- [Validação](VALIDATION.md): checks executados e limitações da validação.

`raw_json` preserva a visão mesclada dos metadados; `catalog_observation` conserva cada objeto original diferente por hash. `musica.album_id` mantém o primeiro álbum conhecido como principal, e `album_musica` todas as associações conhecidas. Relinking usa o ID original como identidade estável. Não há garantia de obter campos que a API não disponibilizar.

```bash
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --release --locked
```

O workflow testa e compila Linux, Windows e macOS e disponibiliza ZIPs nos artefatos. `scripts/build.sh` e `scripts/build.ps1` empacotam o executável do sistema atual. Autenticação real, cofre e importação pessoal exigem seu Client ID e autorização; os testes usam respostas simuladas e servidor HTTP local.

## Referências oficiais

- [PKCE](https://developer.spotify.com/documentation/web-api/tutorials/code-pkce-flow)
- [Redirect URIs](https://developer.spotify.com/documentation/web-api/concepts/redirect_uri)
- [Músicas curtidas](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-tracks)
- [Álbuns salvos](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-albums)
- [Verificação de itens salvos](https://developer.spotify.com/documentation/web-api/reference/check-library-contains)
- [Rate limits](https://developer.spotify.com/documentation/web-api/concepts/rate-limits)
