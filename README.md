# Spotify Library Sync

CLI local em Rust para manter músicas curtidas, álbuns salvos e discografias dos artistas seguidos em SQLite. Após a primeira importação, o comando normal usa uma busca incremental econômica. Os dados podem ser consultados offline.

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

Se a porta estiver ocupada, escolha outra, como `8899`, tanto no `.env` quanto no Dashboard. A URI deve coincidir. O login normal solicita `user-library-read`; `login --discography` acrescenta `user-follow-read`. Ambos usam PKCE com validação de `state`.

Os tokens ficam no cofre de credenciais do sistema, sem fallback para texto puro. No Linux, é necessário um Secret Service desbloqueado, como GNOME Keyring ou KWallet, e uma sessão D-Bus. O aplicativo usa uma conta por banco e confere o ID de `/me` antes de alterar seus dados.

PowerShell, executando a partir do código-fonte:

```powershell
Copy-Item .env.example .env
# Preencha o Client ID em .env.
cargo build --release --locked
.\target\release\spotify-sync.exe login
.\target\release\spotify-sync.exe sync
```

## Atualizar uma instalação anterior

Substitua o executável e mantenha seu `.env` e a pasta `data`. As migrations são aplicadas automaticamente; não é necessário apagar o banco. Para habilitar discografias, refaça o login com `login --discography`; as consultas e sincronizações da biblioteca continuam compatíveis.

- Se a importação anterior terminou, o próximo `sync` será incremental.
- Se há um job v0.1.0 pendente, use `sync --resume`. O modo continua aparecendo como `legacy`, mas a retomada recebe a nova proteção de checkpoints da v0.2.1.
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

### Avisos de limite de requisições

O menu e os comandos apresentam o prazo no fuso local configurado no sistema
(inclusive no Windows/CMD). O deslocamento UTC corresponde à data do prazo;
não há fuso de Brasília fixado nem consulta de fuso pela internet.

Exemplo em um computador configurado para UTC-03:00:

```text
Limite de requisições ativo.
Disponível em: 05/11/2026 às 13:30:11 (UTC-03:00)
Tempo restante: 12 minutos e 34 segundos.
Nenhuma requisição foi enviada.
```

A data usa DD/MM/AAAA, a hora usa 24 horas sem frações de segundo, e o tempo
restante é recalculado ao exibir a mensagem. Frações positivas são arredondadas
para cima; períodos longos incluem dias e horas. A conversão considera a mudança
de dia, mês e ano. Um aviso exibido após o vencimento informa que o prazo encerrou
e mostra zero segundos, sem contagem negativa.

A última linha acima aparece quando um cooldown já salvo bloqueia a operação.
Se o Spotify acabou de responder HTTP 429 ou QUOTA_EXCEEDED, o aviso usa a mesma
data e contagem, mas informa a resposta recebida e a pausa da operação, sem afirmar
que nenhuma requisição foi enviada. Prazo ausente é identificado como não informado.

Os prazos continuam armazenados e comparados em UTC no SQLite. A apresentação
local não altera o bloqueio: antes do vencimento, nenhuma nova chamada da operação
bloqueada é enviada. O vencimento permite tentar retomar; a API pode aplicar outro
limite. As saídas JSON de status mantêm os timestamps UTC para automação.
A prévia e a seleção das playlists LSUA/USSA continuam exclusivamente locais,
disponíveis mesmo durante o cooldown.


Ctrl+C pausa a execução. Se fechar o terminal ou reiniciar o PC, os lotes confirmados permanecem; execute `sync --resume` depois. O aplicativo não permanece rodando quando fechado nem inicia automaticamente com o Windows.

## Retomada da primeira importação (v0.2.1)

A importação completa mantém checkpoints separados para a primeira passagem e para a conferência. Ao retomar a primeira passagem, o aplicativo consulta o offset pendente e procura para trás o último item gravado, comparando ID e `added_at`. Se a posição tiver mudado, também procura próximo da posição ajustada pela variação do total. Reutiliza as páginas já obtidas durante essa busca.

Exemplo: uma importação interrompida no offset 7.450 tenta esse ponto e as páginas próximas; ao encontrar a referência, continua dali. A redução ou o aumento do total não apaga o catálogo nem zera automaticamente a primeira passagem. Pequenas sobreposições são tratadas por UPSERT. Jobs antigos que só armazenavam hashes de página podem usar uma sobreposição de IDs confirmados como referência.

A busca é limitada a 10 chamadas por retomada, incluindo tentativas HTTP repetidas. Se a referência não aparecer dentro desse limite, a execução pausa preservando o checkpoint:

```bash
spotify-sync sync --resume --resume-search-pages 30
```

O limite aceita de 1 a 100 chamadas e é persistido no job. Se todas as referências desaparecerem, será necessário decidir explicitamente entre ampliar a busca e começar outra execução com `--restart`; não há reinício completo silencioso.

Na conferência, uma falha de rede também permite retomar seu próprio checkpoint. Se o cabeçalho ou a posição mudar, somente a conferência é reiniciada. Se o conjunto observado diferir da primeira passagem, os dados novos são incorporados ao catálogo e é solicitada outra conferência, sem repetir a importação. Os estados e as remoções só são publicados depois de obter uma conferência completa compatível com o conjunto anterior.

`status` inclui `full_passes`, com `phase`, `imported_offset` e `verified_offset`. O terminal distingue “importação” de “conferência — importação preservada”. Isso não cria um snapshot no Spotify; uma biblioteca em alteração contínua ainda pode precisar de novas conferências. Um checkpoint já apagado por um executável anterior não pode ser recuperado apenas pelo número que apareceu no terminal.

## Menu no CMD e criação de playlists (v0.4.0)

Execute `spotify-sync` sem argumentos para abrir o menu interativo. No Windows, `abrir-terminal.cmd` abre o programa na pasta correta e mantém o CMD aberto. O menu oferece login, sincronização e retomada da biblioteca, discografias, status, estatísticas e criação/retomada de playlists. Os comandos anteriores continuam disponíveis para scripts.

| Opção | Seleção local | Nome padrão do Spotifast |
|---|---|---|
| LSUA | Músicas curtidas cujo álbum principal não está salvo. | `LSUA - {período}` |
| USSA | Faixas não curtidas vinculadas a pelo menos um álbum salvo. | `USSA - {período}` |

**A seleção usa exclusivamente o SQLite.** Criar uma playlist não sincroniza a biblioteca, não consulta `contains`, não baixa metadados e não percorre artistas ou álbuns no Spotify. Dados ausentes não são buscados automaticamente. Faixas locais, explicitamente indisponíveis ou sem URI válida são excluídas; a seleção é deduplicada por identidade e URI. Não há agrupamento de gravações distintas por nome ou ISRC.

O menu pede período, referência da data, nome e visibilidade. Aceita ano (`2026`), mês/ano (`02/2026`), data (`05/02/2026`) ou intervalo inclusivo; os comandos também aceitam datas ISO. Um único período abrange o ano, mês ou dia inteiro. Sem período, o sufixo é `All`. Os demais nomes seguem o Spotifast: `LSUA - 02-2026`, `USSA - 2025 to 2026` ou `USSA - Through 2026`.

A referência padrão é a data de adição: data da curtida na LSUA e data de salvamento do álbum na USSA, em UTC. Também é possível filtrar pelo lançamento do álbum; datas de lançamento com precisão de ano ou mês representam intervalos que podem se sobrepor ao período escolhido. Nomes são editáveis e playlists começam privadas por padrão.

Antes de criar, a prévia mostra quantidade, até dez músicas, estado da importação, cobertura e datas de atualização/reconciliação da base local. Na USSA também avisa sobre álbuns com faixas incompletas. A criação depende de confirmação, inclusive para aceitar a seleção local parcial. Uma seleção vazia não cria playlist nem job. O usuário pode sair e sincronizar separadamente antes de tentar novamente.

Autorize as permissões novas pelo item de login do menu ou pelo comando:

```bash
spotify-sync login --discography --playlists
```

`--playlists` acrescenta `playlist-modify-private`, `playlist-modify-public` e `playlist-read-private`. O escopo de leitura de playlists permite conferir o envio após uma interrupção. O login normal continua solicitando apenas leitura da biblioteca; as flags são explícitas e precisam ser incluídas ao refazer uma autorização com essas funcionalidades.

Também há comandos diretos:

```bash
# Prévia offline, sem credenciais ou internet; JSON com todas as candidatas.
spotify-sync playlist preview --kind lsua --from 02/2026
spotify-sync playlist preview --kind ussa --from 2025 --to 2026 --date-basis release
# Prévia e confirmação interativa.
spotify-sync playlist create --kind lsua
# Para scripts: confirmação explícita; base incompleta exige a outra flag.
spotify-sync playlist create --kind ussa --yes --allow-incomplete
spotify-sync playlist status
spotify-sync playlist resume
spotify-sync playlist resume --job ID_DO_JOB --request-budget 1500
```

`--name` altera o nome e `--public` escolhe uma playlist pública. Antes do primeiro envio, o programa congela a seleção em um job SQLite, com ID, conta e URIs ordenadas. Retomadas usam essa seleção mesmo se a biblioteca local mudar. O perfil `/me` confere a conta antes de qualquer gravação. A API é usada para criar a playlist e enviar lotes de até 100 músicas; na retomada também pode ler suas playlists e os itens da playlist de destino para conferir uma resposta perdida. Essas leituras não atualizam os dados da biblioteca local.

POSTs com falha de rede, HTTP 5xx ou resposta sem confirmação não são repetidos automaticamente. Se a criação ficou incerta, a retomada procura o marcador do job na descrição, sem criar outra playlist automaticamente. Se o marcador não for localizado, a execução pausa; após conferir manualmente que a playlist não existe, use `spotify-sync playlist retry-create --job ID_DO_JOB`. Esse comando pede confirmação e pode gerar outra playlist se a primeira existir; para scripts, `--yes` assume essa confirmação. Rejeições explícitas, como HTTP 403/429, permitem uma nova tentativa normal após resolver a causa/cooldown.

Após um envio incerto, a retomada confere o prefixo remoto contra o plano congelado. Um lote já observado como enviado não é repetido. Conteúdo divergente ou lote parcialmente observado pausa a execução sem sobrescrever a playlist. Evite editar a playlist enquanto o job estiver pendente. A API não oferece uma transação única junto com SQLite; visibilidade atrasada ou alterações concorrentes no Spotify impedem uma garantia absoluta de envio exatamente uma vez.

O orçamento padrão é de 1.000 tentativas HTTP por invocação de criação/retomada, incluindo perfil, conferência, POSTs e retries de leituras. Aceita `--request-budget` entre 5 e 10.000. Uma conferência de playlist grande também consome esse limite; aumente-o se a retomada pausar antes de terminar a conferência. Intervalo entre chamadas, cooldown global, lock entre processos e Ctrl+C são respeitados. Fechar o programa não apaga os checkpoints; nenhuma execução continua em segundo plano.

## Discografias dos artistas seguidos (v0.3.0)

```bash
spotify-sync login --discography
spotify-sync discography sync
spotify-sync discography status
# Se pausar por orçamento, fechar o terminal ou reiniciar o PC:
spotify-sync discography sync --resume
```

A fila nasce exclusivamente da lista de artistas que você segue. Participantes encontrados nos metadados são cadastrados, mas não ampliam a fila. O catálogo inclui álbuns, singles, compilações e participações (`appears_on`). Importar uma discografia não curte músicas nem salva álbuns na sua conta ou no estado local da biblioteca.

Na primeira passagem de cada artista, toda a sua listagem é percorrida. Depois, o cache da listagem vale **7 dias**. Quando vence, a busca começa nas páginas recentes e para após duas páginas inteiramente conhecidas ou no fim. A referência é congelada antes da coleta; os itens recém-importados não provocam uma parada prematura. Essa atualização é uma heurística de janela recente: relançamentos ou alterações antigas fora dela podem exigir `--full`.

Por padrão, um novo job seleciona até **100 artistas**, priorizando os que nunca tiveram uma listagem completa, depois os verificados há mais tempo. Com 1.000 seguidos, conclua e inicie novos jobs para processar os próximos lotes. A seleção fica congelada durante a retomada. Não há busca automática em segundo plano: um álbum novo é detectado na próxima execução em que seu artista estiver elegível pelo TTL, ou em uma busca explícita desse artista.

| Comando | Finalidade |
|---|---|
| `discography sync --artist ID` | Consultar um artista seguido agora, ignorando seu TTL. |
| `discography sync --artist ID --full` | Percorrer toda a listagem desse artista, reutilizando faixas completas. |
| `discography sync --ttl-days 0` | Tornar todos os artistas elegíveis, respeitando o limite do lote. |
| `discography sync --max-artists 1000 --catalog-only` | Descobrir lançamentos de até 1.000 artistas e deixar as faixas na fila. |
| `discography tracks` | Importar as faixas pendentes dos grupos/mercado informados, sem consultar novamente as listagens. |
| `discography sync --groups album,single` | Limitar a descoberta a álbuns e singles. |
| `discography sync --market BR` | Informar um mercado na consulta à API. |
| `discography artists` | Listar artistas seguidos e cobertura do catálogo local. |
| `discography albums --artist ID` | Listar os lançamentos conhecidos desse artista, inclusive participações. |
| `discography restart` | Arquivar o job interrompido, preservando catálogo e fila de faixas; depois inicie outro job. |

Os comandos da tabela usam o prefixo `spotify-sync`. Consultas offline aceitam `--limit` e `--offset`. `--artist` recebe IDs Spotify, separados por vírgula ou repetindo a opção, e aceita apenas artistas seguidos. Se houver um job pendente, retome ou arquive antes de iniciar outro.

O limite padrão é de **200 tentativas HTTP por invocação**, contando perfil, páginas de seguidos, lançamentos, faixas e retries. Use `--request-budget 500` para outro limite, inclusive com `--resume`; os demais parâmetros de uma retomada permanecem os do job original. O orçamento aceita 5 a 10.000 tentativas. A lista de 1.000 seguidos custa 20 páginas de até 50 artistas; o perfil também consome uma tentativa por invocação. Lançamentos usam páginas de até 10 e faixas de até 50, portanto a primeira importação pode exigir várias retomadas.

As etapas são persistentes: confirmar seguidos, listar lançamentos e importar faixas. Só uma lista de seguidos completa e válida publica quem deixou de ser seguido. As faixas são compartilhadas por ID de álbum e contexto de conta/mercado, mesmo quando o mesmo álbum aparece para vários artistas. O cache de faixas completo da biblioteca também é reutilizado no mercado padrão. Uma quantidade de faixas alterada invalida somente o álbum afetado. Mudar o filtro de grupos cria outra cobertura de listagem, mas reutiliza as faixas do mesmo contexto de mercado. Ao separar descoberta e faixas com opções personalizadas, repita `--groups` e `--market` no comando `discography tracks`; por exemplo, `discography sync --catalog-only --groups album,single --market BR` e depois `discography tracks --groups album,single --market BR`. O país da conta pode prevalecer sobre o mercado solicitado conforme as regras da API.

Deixar de seguir um artista preserva seu catálogo e exclui esse artista de novas seleções e do processamento de faixas ligadas apenas a ele. `discography tracks` usa a última lista de seguidos confirmada; execute um novo `discography sync` para atualizá-la. Se a paginação mudar na retomada, somente a listagem do artista afetado é reposicionada. Reiniciar um job incompleto exige uma listagem completa dos artistas cujo cache anterior foi parcialmente atualizado, evitando usar esses itens como uma referência de parada falsa.

`discography status` distingue listagens concluídas de álbuns com faixas pendentes, informa orçamento, etapa e motivo da pausa. `discography artists` mostra datas, cobertura e `known_catalog_tracks_complete`, que se refere somente ao catálogo conhecido no contexto atual. Concluir essa fila não estabelece uma importação completa de curtidas ou álbuns salvos. O cooldown de quota é compartilhado com a sincronização da biblioteca; limite de tentativas e intervalo entre chamadas reduzem o consumo, mas não garantem disponibilidade da quota do Spotify.

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
- `src/legacy_sync.rs`: compatibilidade com modos completos e jobs v0.1.0.
- `src/full_scan.rs`: importação e conferência completas com checkpoints independentes e busca de referências na retomada.
- `src/playlist.rs`: seleção exclusivamente local, filtros de período, envio e recuperação de playlists.
- `src/menu.rs`: menu e confirmações pelo terminal.
- `src/discography.rs`: filas de artistas seguidos, cobertura de lançamentos, cache compartilhado e orçamentos de discografia.
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

- [Artistas seguidos](https://developer.spotify.com/documentation/web-api/reference/get-followed)
- [Lançamentos do artista](https://developer.spotify.com/documentation/web-api/reference/get-an-artists-albums)
- [Faixas do álbum](https://developer.spotify.com/documentation/web-api/reference/get-an-albums-tracks)

- [Criação de playlists](https://developer.spotify.com/documentation/web-api/reference/create-playlist)
- [Adicionar itens à playlist](https://developer.spotify.com/documentation/web-api/reference/add-items-to-playlist)
- [Conferir itens da playlist](https://developer.spotify.com/documentation/web-api/reference/get-playlists-items)
