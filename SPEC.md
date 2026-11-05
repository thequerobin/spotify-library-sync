# SPEC — Spotify Library Sync

**Versão:** 1.0  
**Linguagem:** Rust  
**Banco de dados:** SQLite  
**Integração:** Spotify Web API  
**Tipo:** Aplicativo local, inicialmente CLI  
**Objetivo:** Sincronização e persistência da biblioteca pessoal do Spotify.

---

## 1. Objetivo

Desenvolver um aplicativo em Rust para autenticar uma conta Spotify, recuperar todas as músicas curtidas e todos os álbuns salvos e persistir os dados localmente em SQLite.

O aplicativo deve preservar o máximo possível de metadados disponibilizados pela Spotify Web API e manter relações consistentes entre músicas, álbuns e artistas.

O sistema deverá permitir sincronizações futuras sem recriar o banco de dados e sem realizar requisições desnecessárias.

O banco será a fonte local para consultas e processamento, mas o Spotify continuará sendo a fonte de verdade para o estado de itens salvos.

## 2. Escopo funcional

### RF01 — Autenticação

- Utilizar OAuth 2.0 Authorization Code com PKCE.
- Abrir o navegador para autenticação.
- Utilizar callback local em `127.0.0.1`.
- Solicitar o escopo `user-library-read`.
- Renovar automaticamente o token de acesso.
- Armazenar credenciais sensíveis em armazenamento seguro do sistema, não em texto puro no SQLite.
- Identificar a conta autenticada e impedir a mistura de dados entre contas.

### RF02 — Importação das músicas curtidas

Buscar todas as músicas da biblioteca do usuário utilizando:

`GET /me/tracks`

Requisitos:

- Implementar paginação até recuperar todos os registros.
- Utilizar o maior tamanho de página permitido pelo endpoint.
- Armazenar os metadados de cada música.
- Registrar a data em que ela foi adicionada à biblioteca.
- Definir `is_saved = true`.
- Identificar o álbum ao qual a música pertence.
- Criar o álbum no banco caso ainda não exista.
- Preservar os artistas associados.
- Impedir registros duplicados.

### RF03 — Importação dos álbuns salvos

Buscar todos os álbuns da biblioteca utilizando:

`GET /me/albums`

Requisitos:

- Implementar paginação completa.
- Registrar todos os metadados disponíveis.
- Registrar a data em que cada álbum foi salvo.
- Definir `is_saved = true`.
- Atualizar álbuns que já existam por terem sido identificados nas músicas curtidas.
- Não criar registros duplicados.
- Preservar as informações de artistas, imagens e demais metadados.

### RF04 — Importação das músicas dos álbuns salvos

Para cada álbum salvo, recuperar sua lista completa de músicas.

Regras:

- Utilizar primeiramente as faixas já presentes na resposta do álbum.
- Buscar páginas adicionais somente quando necessário.
- Criar as músicas ausentes no banco.
- Associar corretamente cada música ao álbum.
- Inicialmente definir `is_saved = false` para músicas ainda não identificadas na lista de curtidas.
- Não confundir álbum salvo com música curtida: salvar um álbum não significa que todas as suas músicas estejam individualmente salvas.
- Evitar requisições adicionais para metadados já disponíveis.

### RF05 — Sincronização

Disponibilizar os seguintes modos:

**Sincronização inicial:** importa toda a biblioteca e constrói o banco local.

**Sincronização normal:** reconcilia a biblioteca local com os dados atuais do Spotify, reutilizando os metadados existentes quando possível.

**Sincronização completa:** revalida os registros e seus estados, incluindo remoções, mantendo dados históricos locais.

A sincronização deverá:

- Utilizar UPSERT.
- Manter identificadores estáveis.
- Não duplicar músicas ou álbuns.
- Detectar adições e remoções.
- Registrar a última sincronização bem-sucedida.
- Permitir retomada após interrupções.
- Evitar baixar novamente metadados imutáveis sem necessidade.

Uma sincronização incremental baseada em data poderá ser utilizada como otimização, mas não substituirá a reconciliação periódica completa, pois o Spotify não fornece um feed universal de alterações e exclusões dessa biblioteca.

### RF06 — Progresso e recuperação

O aplicativo deve registrar persistentemente:

- Etapa atual.
- Total de itens conhecidos.
- Quantidade processada.
- Próxima página pendente.
- Álbum atualmente em processamento.
- Requisições realizadas.
- Quantidade de erros e tentativas.
- Horário de atualização.
- Situação da execução.

Estados possíveis:

`pending`, `running`, `paused`, `rate_limited`, `failed`, `completed`.

Se o processo for encerrado ou o computador reiniciar, o aplicativo deverá conseguir continuar a execução interrompida.

Os checkpoints devem ser persistidos somente após confirmar a gravação dos dados de cada lote.

### RF07 — Consultas locais

Permitir consultas como:

- Todas as músicas curtidas.
- Todos os álbuns salvos.
- Músicas curtidas cujos álbuns não estão salvos.
- Álbuns salvos com músicas não curtidas.
- Músicas de determinado artista.
- Músicas por data de lançamento.
- Músicas por data em que foram curtidas.
- Álbuns por data em que foram salvos.
- Estatísticas da biblioteca.

Nenhuma dessas consultas deve exigir requisições ao Spotify quando as informações já estiverem no banco.

---

## 3. Modelo de dados

### 3.1. Tabela `album`

Armazena os álbuns salvos e os álbuns identificados por meio das músicas curtidas.

Campos:

| Campo | Tipo | Descrição |
|---|---|---|
| id | TEXT PK | Spotify ID do álbum |
| name | TEXT | Nome |
| album_type | TEXT | album, single ou compilation |
| total_tracks | INTEGER | Número de músicas |
| release_date | TEXT | Data original do lançamento |
| release_date_precision | TEXT | year, month ou day |
| spotify_uri | TEXT | URI Spotify |
| spotify_url | TEXT | URL pública |
| href | TEXT | Endpoint do objeto |
| image_url | TEXT | URL da capa principal |
| label | TEXT | Gravadora, quando disponível |
| popularity | INTEGER | Popularidade, caso disponível |
| upc | TEXT | Identificador UPC |
| ean | TEXT | Identificador EAN |
| is_saved | INTEGER | 0 ou 1 |
| saved_at | TEXT | Data em que o usuário salvou |
| last_seen_saved_sync | TEXT | Última verificação em biblioteca |
| metadata_fetched_at | TEXT | Data de atualização dos metadados |
| created_at | TEXT | Criação local |
| updated_at | TEXT | Atualização local |
| raw_json | TEXT | JSON integral recebido da API |

O campo `raw_json` deverá preservar atributos não mapeados, inclusive estruturas aninhadas, imagens, direitos autorais, restrições e outros metadados fornecidos pela API.

Campos removidos ou depreciados pelo Spotify deverão ser opcionais.

### 3.2. Tabela `musica`

Armazena as músicas curtidas e as músicas importadas dos álbuns salvos.

| Campo | Tipo | Descrição |
|---|---|---|
| id | TEXT PK | Spotify ID da música |
| album_id | TEXT FK | Álbum associado |
| name | TEXT | Nome da música |
| duration_ms | INTEGER | Duração |
| track_number | INTEGER | Número da faixa |
| disc_number | INTEGER | Número do disco |
| explicit | INTEGER | Conteúdo explícito |
| is_local | INTEGER | Música local |
| is_playable | INTEGER | Disponibilidade, quando informada |
| spotify_uri | TEXT | URI Spotify |
| spotify_url | TEXT | Link Spotify |
| href | TEXT | Endpoint |
| isrc | TEXT | Identificador ISRC, quando disponível |
| popularity | INTEGER | Popularidade, quando disponível |
| preview_url | TEXT | Prévia, quando disponível |
| is_saved | INTEGER | 0 ou 1 |
| saved_at | TEXT | Data em que foi curtida |
| last_seen_saved_sync | TEXT | Última confirmação |
| metadata_fetched_at | TEXT | Última atualização dos metadados |
| created_at | TEXT | Criação local |
| updated_at | TEXT | Atualização local |
| raw_json | TEXT | JSON original completo |

O relacionamento principal será:

`musica.album_id → album.id`

**Regra:** `is_saved` representa exclusivamente se a música foi adicionada individualmente à biblioteca do usuário.

O aplicativo não deve definir esse campo como verdadeiro apenas porque seu álbum está salvo.

### 3.3. Tabela `artista`

Para não duplicar dados dos artistas em todas as músicas e álbuns:

- `id`
- `name`
- `spotify_uri`
- `spotify_url`
- `href`
- `raw_json`
- `created_at`
- `updated_at`

### 3.4. Tabelas de relacionamento

`musica_artista`:

- `musica_id`
- `artista_id`
- `position`

`album_artista`:

- `album_id`
- `artista_id`
- `position`

`album_musica`:

- `album_id`
- `musica_id`
- `disc_number`
- `track_number`

Chave primária composta para cada tabela de relacionamento.

A tabela `album_musica` preserva o vínculo de catálogo entre álbuns e músicas, inclusive quando uma faixa aparece associada a mais de um lançamento.

O `album_id` da tabela `musica` representa seu álbum principal, enquanto `album_musica` representa todas as associações conhecidas.

### 3.5. Tabela `sync_job`

Persistência do progresso:

- `id`
- `job_type`
- `status`
- `stage`
- `next_offset`
- `total_items`
- `processed_items`
- `request_count`
- `retry_count`
- `last_error`
- `started_at`
- `updated_at`
- `completed_at`

### 3.6. Tabela `sync_album_queue`

Fila persistente dos álbuns cujas faixas precisam ser importadas ou verificadas:

- `album_id`
- `status`
- `next_offset`
- `attempts`
- `last_error`
- `updated_at`

### 3.7. Tabela `sync_seen_item`

Registra quais itens foram observados em uma execução de sincronização:

- `job_id`
- `item_type`
- `item_id`

Serve para detectar itens removidos da biblioteca sem apagar metadados históricos.

---

## 4. Regras do banco SQLite

O banco será armazenado em:

`./data/spotify_library.db`

Requisitos:

- Criar o diretório automaticamente.
- Ativar `PRAGMA foreign_keys = ON`.
- Utilizar WAL quando apropriado.
- Aplicar migrations versionadas.
- Criar índices para `is_saved`, `album_id`, `saved_at` e `release_date`.
- Criar índices para as tabelas de relacionamento.
- Utilizar transações para salvar lotes e checkpoints de forma atômica.
- Utilizar `ON CONFLICT DO UPDATE` para atualizações.
- Nunca sobrescrever metadados completos com respostas parciais ou valores nulos de objetos simplificados.
- Separar o estado de biblioteca dos metadados de catálogo.
- Usar timestamps UTC em formato consistente.
- Preservar a precisão original das datas de lançamento.

Não apagar automaticamente músicas e álbuns que deixaram de estar salvos. Atualizar `is_saved` para falso, preservando os demais dados.

Não alterar o estado `is_saved` com base em uma sincronização incompleta ou com falha.

---

## 5. Arquitetura Rust

Estrutura proposta:

```text
spotify-library-sync/
├── Cargo.toml
├── .env.example
├── .gitignore
├── migrations/
│   └── 0001_initial.sql
├── data/
│   └── spotify_library.db
├── src/
│   ├── main.rs
│   ├── config.rs
│   ├── error.rs
│   ├── auth/
│   │   ├── mod.rs
│   │   └── oauth.rs
│   ├── spotify/
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   ├── models.rs
│   │   ├── pagination.rs
│   │   └── rate_limiter.rs
│   ├── database/
│   │   ├── mod.rs
│   │   ├── albums.rs
│   │   ├── tracks.rs
│   │   ├── artists.rs
│   │   └── sync.rs
│   ├── sync/
│   │   ├── mod.rs
│   │   ├── manager.rs
│   │   ├── saved_tracks.rs
│   │   ├── saved_albums.rs
│   │   ├── album_tracks.rs
│   │   └── reconciliation.rs
│   └── cli/
│       ├── mod.rs
│       └── commands.rs
└── tests/
    ├── database_tests.rs
    └── sync_tests.rs
```

### Bibliotecas sugeridas

| Biblioteca | Finalidade |
|---|---|
| tokio | Execução assíncrona |
| reqwest | Cliente HTTP |
| serde / serde_json | Serialização |
| sqlx | SQLite e migrations |
| clap | CLI |
| indicatif | Barras de progresso |
| tracing | Logs |
| anyhow / thiserror | Tratamento de erros |
| chrono | Datas |
| keyring | Armazenamento de tokens |
| dotenvy | Configuração local |

Preferir Rust estável e versões compatíveis atualizadas das dependências.

---

## 6. Fluxo de sincronização

### Etapa 1 — Inicialização

1. Carregar configurações.
2. Inicializar SQLite.
3. Executar migrations.
4. Recuperar credenciais.
5. Validar autenticação.
6. Identificar sincronizações interrompidas.
7. Retomar execução existente ou iniciar nova.

### Etapa 2 — Músicas curtidas

1. Consultar `/me/tracks`.
2. Percorrer todas as páginas.
3. Persistir os álbuns identificados.
4. Persistir os artistas.
5. Persistir as músicas com `is_saved = true`.
6. Salvar os vínculos.
7. Atualizar o checkpoint.

### Etapa 3 — Álbuns salvos

1. Consultar `/me/albums`.
2. Percorrer todas as páginas.
3. Persistir os metadados dos álbuns.
4. Definir `album.is_saved = true`.
5. Persistir artistas e relacionamentos.
6. Adicionar à fila os álbuns cujas faixas ainda não foram completamente importadas.

### Etapa 4 — Faixas dos álbuns

1. Consumir a fila persistida.
2. Reutilizar faixas já retornadas pela consulta de álbuns.
3. Consultar páginas adicionais somente quando necessário.
4. Inserir músicas ausentes.
5. Criar seus relacionamentos.
6. Preservar `is_saved = true` das músicas já confirmadas como curtidas.
7. Registrar o processamento concluído.

### Etapa 5 — Reconciliação

Após concluir com sucesso todas as páginas da coleção correspondente:

- Marcar como `is_saved = false` os itens que não foram encontrados na listagem completa atual.
- Manter os dados históricos.
- Atualizar o estado da sincronização.
- Confirmar que a contagem de itens processados corresponde à listagem obtida.
- Não efetuar reconciliação destrutiva após falha ou interrupção.

Se a biblioteca sofrer alterações enquanto ocorre a paginação, a reconciliação deve detectar possíveis inconsistências e permitir nova verificação.

### Etapa 6 — Finalização

Exibir:

- Quantidade de músicas curtidas.
- Quantidade de álbuns salvos.
- Quantidade total de músicas cadastradas.
- Quantidade total de álbuns cadastrados.
- Novas músicas e álbuns.
- Itens que deixaram de estar salvos.
- Quantidade de requisições.
- Erros e pendências.
- Status final.

---

## 7. Rate Limiter e controle de quota

Implementar um gerenciador centralizado para todas as chamadas à API.

Requisitos:

- Limitar a concorrência de requisições.
- Utilizar uma fila de prioridades.
- Respeitar o header `Retry-After`.
- Utilizar backoff exponencial com jitter para falhas transitórias.
- Tratar renovação de token.
- Persistir o progresso antes de pausar.
- Distinguir rate limit temporário de quota esgotada.
- Evitar ciclos infinitos de repetição.
- Não utilizar novas credenciais para tentar contornar quotas.
- Permitir configuração do ritmo de requisições.
- Exibir o consumo de requisições conhecido pelo aplicativo.

Se a resposta indicar `QUOTA_EXCEEDED`:

1. Pausar as novas solicitações.
2. Persistir o checkpoint.
3. Registrar o motivo.
4. Manter os dados já importados.
5. Permitir retomar posteriormente.
6. Não realizar tentativas contínuas e agressivas.

A quota é controlada pelo Spotify, portanto o aplicativo não pode garantir que nunca será atingida. Deve, entretanto, reduzir o consumo e lidar corretamente com o esgotamento.

### Otimizações obrigatórias

- Usar paginação máxima suportada.
- Evitar consultar cada música individualmente.
- Reaproveitar os objetos retornados nos endpoints de biblioteca.
- Deduplicar IDs antes de enfileirar operações.
- Não consultar novamente álbuns já completos sem necessidade.
- Agrupar operações onde a API permitir.
- Priorizar a sincronização dos estados da biblioteca antes de consultas complementares de metadados.
- Reutilizar o cache local para consultas.
- Configurar TTL de metadados separadamente da atualização do estado de itens salvos.

---

## 8. Interface CLI

Comandos planejados:

```bash
spotify-sync login

spotify-sync sync

spotify-sync sync --full

spotify-sync sync --resume

spotify-sync status

spotify-sync stats

spotify-sync albums --saved

spotify-sync tracks --liked

spotify-sync tracks --liked --album-not-saved
```

Deve exibir barras de progresso por etapa e quantidade processada.

O usuário precisa conseguir identificar claramente se o aplicativo está buscando músicas, álbuns ou faixas de álbuns.

---

## 9. Requisitos não funcionais

### Desempenho

- Suportar bibliotecas com dezenas de milhares de músicas.
- Evitar carregar todos os itens simultaneamente na memória.
- Processar dados em lotes.
- Utilizar índices apropriados.
- Minimizar chamadas à API.

### Confiabilidade

- Permitir retomada após encerramento inesperado.
- Garantir idempotência.
- Utilizar transações.
- Proteger contra duplicidades.
- Não perder dados previamente sincronizados.
- Implementar testes de migração, persistência e reconciliação.

### Segurança

- Não armazenar tokens em logs.
- Não versionar o banco pessoal nem credenciais.
- Utilizar `.gitignore` adequado.
- Solicitar apenas as permissões necessárias.
- Utilizar PKCE e validação do parâmetro `state`.

### Manutenção

- Separar cliente HTTP da lógica de negócio.
- Separar persistência da sincronização.
- Utilizar migrations.
- Adotar interfaces que permitam acrescentar novas fontes ou funcionalidades.
- Centralizar o gerenciamento de erros e requisições.

---

## 10. Critérios de aceite

A implementação será considerada concluída quando:

- [ ] A autenticação Spotify funcionar.
- [ ] Todas as músicas curtidas forem importadas.
- [ ] Todos os álbuns salvos forem importados.
- [ ] Os álbuns de músicas curtidas forem cadastrados, mesmo quando não salvos.
- [ ] Todas as faixas dos álbuns salvos forem cadastradas.
- [ ] Os relacionamentos entre músicas, álbuns e artistas estiverem corretos.
- [ ] O estado `is_saved` das músicas for independente do estado dos álbuns.
- [ ] O SQLite persistir os dados entre execuções.
- [ ] A sincronização repetida não gerar duplicidades.
- [ ] A remoção de uma curtida ou álbum salvo for refletida corretamente após reconciliação.
- [ ] O aplicativo retomar uma sincronização interrompida.
- [ ] O tratamento de 429 e `QUOTA_EXCEEDED` funcionar.
- [ ] Uma falha não marcar indevidamente itens como removidos.
- [ ] As consultas locais funcionarem offline.
- [ ] Os testes automatizados principais passarem.

---

## 11. Fora do escopo inicial

- Interface gráfica.
- Reprodução ou download de músicas.
- Criação automática de playlists.
- Modificação da biblioteca Spotify.
- Sincronização em tempo real.
- Importação de playlists.
- Suporte simultâneo a múltiplas contas.

Esses recursos poderão ser adicionados posteriormente sem reestruturar completamente o núcleo do aplicativo.

## 12. Resultado esperado

Ao executar a sincronização, o projeto deverá possuir um banco SQLite com um catálogo local consistente de músicas, álbuns e artistas associados à biblioteca Spotify do usuário.

A solução deverá ser preparada para servir como base de funcionalidades futuras, principalmente análises da biblioteca, geração de playlists, identificação de álbuns não salvos e processamento offline.
