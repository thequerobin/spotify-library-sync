# Validação — v0.3.0

Data: 05/11/2026. Ambiente local: Linux x86_64, Rust 1.99.1.

- `cargo fmt -- --check`: aprovado.
- `cargo clippy --locked --all-targets -- -D warnings`: aprovado, sem avisos.
- `cargo test --locked --all-targets`: **69 testes**, sem falhas.
- `cargo build --release --locked`: compilação Linux aprovada.
- `cargo build --release --locked --target x86_64-pc-windows-gnu`: compilação Windows x64 aprovada por cross-compilation com MinGW.
- Executável Windows: formato PE32+ x86-64 e imports de DLLs inspecionados; somente bibliotecas do sistema.
- Smoke test do release Linux: versão, ajuda, status e consultas locais, sem credenciais.
- Banco criado por release: sete migrations aplicadas, `PRAGMA integrity_check` igual a `ok`.

## Cenários cobertos

### Transporte HTTP — 7 testes

429 sem JSON com Retry-After; QUOTA_EXCEEDED em 403; retry de 503 com HTML; bloqueio de redirect de credenciais; orçamento limitando retries reais; query tipada e codificada de `contains`; parâmetros reais de paginação, mercado e grupos para os endpoints de discografia.

### CLI — 2 testes

Consultas offline com datas, artista, parâmetros vinculados e paginação; lock entre processos preservando a disponibilidade de status.

### Incremental e migração — 16 testes

- 21.948 curtidas e álbuns sem novidades, com cinco chamadas contando o perfil.
- Cinco músicas novas no início, com parada após três páginas e uma conferência limitada.
- Totais iguais, timestamps empatados e item já cadastrado sem estar curtido.
- Atualização da data ao curtir novamente um ID conhecido.
- Orçamento, reabertura do SQLite e retomadas que avançam além da primeira janela.
- Falha em álbuns sem repetir a coleta de curtidas já publicada.
- Mudança concorrente no cabeçalho sem publicação prematura ou full automático.
- Ordem incompatível sem avanço da fronteira.
- Reconciliação explícita de remoções e bloqueio da reconciliação destrutiva no modo incremental.
- Verificação direcionada em lotes de 40, validação de resposta e URIs desconhecidas sem enriquecimento automático.
- Preservação da fila após restart, sem reintroduzir cópias concluídas.
- Migração real de schema v0.1.0, reconhecimento da base completa e seleção de job legado pendente.
- Banco vazio iniciando importação inicial e estabelecendo sua base.
- Cooldown de consulta direcionada persistindo sem um job de sincronização.
- Fila de faixas com orçamento independente, mantendo coleções publicadas e retomando só as faixas restantes.
- Atualização de álbum observado cuja quantidade de faixas mudou.

### Protocolo, regressões e retomada completa — 23 testes

Três testes de PKCE, Retry-After, quota e paginação, além de vinte regressões da importação inicial, UPSERT, cache, reconciliação, histórico, rollback, contas, retomada, JSON original e vínculos entre artistas, músicas e álbuns.

Os sete cenários adicionais da v0.2.1 cobrem: retomada no offset pendente com busca para trás; jobs antigos sem referência ordenada; deslocamento por adições no início; limite de busca sem apagar progresso; interrupção da conferência com ambos os checkpoints preservados; exclusão de referências de fronteira; mudança do total durante a conferência. O teste de alteração com total constante também confirma que somente a conferência é reiniciada.

### Discografias e permissões — 21 testes

Dezenove cenários de integração: apenas artistas seguidos; álbuns compartilhados; reutilização de cache; paginação completa; novos álbuns com atualização recente; orçamento e reabertura do banco; lista de seguidos interrompida; descoberta sem faixas; retomada de faixas; isolamento de mercado e grupos; preservação ao deixar de seguir; resposta malformada sem falsas remoções; quota e cooldown global; 1.000 artistas com rotação de lotes de 100; listagem completa explícita sem refazer faixas; seleção de seguidos e isolamento de conta; alteração de quantidade de faixas; restart de atualização parcial; status sem falsa completude; mudança de catálogo durante retomada preservando outros artistas.

Dois testes de autenticação cobrem a leitura de tokens antigos sem o campo de escopo e a verificação exata da permissão `user-follow-read`. A autorização real continua dependendo da conta pessoal.

## Limitações da validação local

O login real e a leitura da conta pessoal precisam de Client ID e autorização no navegador. Os testes usam dados simulados e um servidor HTTP local. O executável Windows foi compilado, mas não executado em uma sessão Windows neste ambiente. O workflow do GitHub executa testes nativos e builds em Linux, Windows e macOS; seu resultado deve ser consultado separadamente nos Actions.

O incremental é uma heurística de cobertura recente. Os testes não transformam a paginação do Spotify em um snapshot nem garantem detectar alterações antigas fora das páginas consultadas.

A atualização recente de discografias também é heurística; a cobertura completa da listagem exige percorrê-la até o fim. O teste com 1.000 artistas usa respostas simuladas e não constitui um benchmark da quota ou da latência real do Spotify.
