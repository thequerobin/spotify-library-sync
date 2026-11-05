# SPEC de ajuste — sincronização incremental econômica

**Projeto:** Spotify Library Sync, Rust + SQLite.
**Versão proposta:** 1.1 da especificação, sobre a implementação v0.1.0.
**Data:** 05/11/2026.
**Entrega solicitada:** especificação; este documento não altera o executável existente.

## 1. Problema e resultado esperado

A versão atual percorre integralmente `/me/tracks` e `/me/albums` e depois repete as listagens para conferência. Com 21.948 curtidas, são 439 páginas de importação e 439 de verificação somente para músicas. Isso não é adequado como rotina para buscar poucas novidades.

Após a primeira importação completa, `spotify-sync sync` deve passar a procurar adições recentes e parar ao reencontrar uma região já confirmada da biblioteca. Não deve varrer o catálogo inteiro nem repetir toda a leitura por padrão.

A atualização diária terá cobertura limitada às páginas consultadas. Remoções antigas e alterações fora dessa região serão verificadas por comandos explícitos. Essa separação é uma decisão de produto para reduzir requisições.

## 2. Contrato da API e limite da proposta

A documentação consultada de `/me/tracks` e `/me/albums` apresenta paginação por `limit` e `offset`, com até 50 itens por página, e retorna `added_at`. Não documenta um parâmetro `since` para essas coleções. Não inventar esse parâmetro.

Também não há, nas referências desses dois endpoints, uma garantia explícita de ordenação temporal ou opção `sort`. Portanto, a parada em uma região conhecida é uma **heurística baseada na ordem observada**, e não uma garantia de que todas as mudanças foram recuperadas.

Requisitos derivados dessa limitação:

- Não prometer detecção exata de todas as alterações com poucas chamadas.
- Não usar igualdade de totais como prova de ausência de mudanças.
- Não transformar ausência em páginas recentes em evidência de remoção.
- Não usar a data de lançamento como cursor; usar o estado salvo e `added_at`.
- Não fazer varredura completa silenciosa quando a heurística não convergir.
- Informar a cobertura no status e nas consultas que dependem do estado salvo.

Fontes oficiais: [músicas curtidas](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-tracks) e [álbuns salvos](https://developer.spotify.com/documentation/web-api/reference/get-users-saved-albums).

## 3. Modos e comandos

| Comando | Comportamento proposto |
|---|---|
| `spotify-sync sync` com banco ainda sem importação completa | Importação inicial completa; mantém checkpoints existentes. |
| `spotify-sync sync` com base inicial concluída | Busca incremental nas páginas recentes. |
| `spotify-sync sync --resume` | Retoma o modo e a etapa persistidos da execução pendente. |
| `spotify-sync sync --reconcile` | Reconciliação completa dos estados salvos, incluindo remoções; reutiliza metadados e faixas já completos. |
| `spotify-sync sync --full` | Mantém o significado anterior: reconciliação completa e reimportação das faixas dos álbuns salvos. |
| `spotify-sync sync --restart` | Descarta o trabalho pendente de descoberta e inicia outra tentativa, preservando catálogo e estados publicados; respeita cooldown. |
| `spotify-sync check-saved --uri URI ...` | Confirma o estado atual somente dos itens explicitamente indicados. |
| `spotify-sync status` | Exibe modo, cobertura, checkpoints, limites e pendências. |

`--reconcile` e `--full` não podem ser executados implicitamente por um TTL vencido, queda do total, falta de âncora ou mudança na biblioteca. Se a base inicial estiver completa, uma execução normal continua econômica mesmo após meses sem uso.

A reconciliação pode ser recomendada no status quando estiver antiga, mas não será executada automaticamente. Nenhum agendamento é criado por este ajuste.

## 4. Algoritmo incremental por coleção

Executar separadamente para curtidas e álbuns salvos.

### 4.1. Congelar a referência local

No início da coleta de uma coleção, persistir uma referência imutável dos IDs que estavam confirmados como salvos e dos respectivos `saved_at`. Essa referência deve ser independente dos itens gravados pelo job em andamento.

É permitido copiar IDs e datas para uma tabela temporária persistente em SQLite. Ler dados locais não gera requisições ao Spotify. Não copiar JSONs completos para essa referência.

Uma linha existente no catálogo com `is_saved=false` não é um item confirmado. Faixas presentes apenas porque seu álbum foi importado também não contam como curtidas conhecidas.

### 4.2. Ler do início e comparar

1. Solicitar a primeira página, com `limit=50` e `offset=0`.
2. Validar formato, IDs e datas; aplicar a normalização existente de `linked_from`.
3. Para cada item, comparar com a referência congelada **antes** de gravar o lote.
4. Tratar como novidade qualquer ID desconhecido na referência, estado anteriormente falso ou `added_at` diferente.
5. Fazer UPSERT do catálogo e dos relacionamentos; registrar os estados candidatos no job.
6. Confirmar lote, observações e checkpoint na mesma transação.
7. Continuar até uma condição de parada ou esgotamento do orçamento.

Metadados simplificados continuam sem apagar valores completos já conhecidos. Preservar o JSON original e a independência entre `musica.is_saved` e `album.is_saved`.

### 4.3. Condição de parada

Padrão: encerrar a descoberta após **duas páginas consecutivas inteiramente conhecidas**, sem novidades nem mudanças de `added_at`, comparadas à referência congelada.

Condições adicionais:

- As páginas devem ser consecutivas na paginação, sem repetir o mesmo intervalo para atingir o limiar.
- Encontrar somente o primeiro ID conhecido não é suficiente.
- Qualquer novidade reinicia o contador de páginas conhecidas.
- Se o endpoint terminar antes disso, encerrar pelo fim real da coleção.
- Datas iguais devem ser comparadas por ID e timestamp; nunca parar apenas porque `added_at <= watermark`.
- Um watermark temporal é auxiliar, não substitui a referência por identidade e estado.
- A igualdade de `total` com o banco não autoriza pular a primeira página.

Exemplo: primeira página com 5 músicas novas e 45 conhecidas; segunda e terceira páginas inteiramente conhecidas. A coleta para após a terceira página, sem ler as outras centenas.

Mesmo duas páginas conhecidas não provam que não existe uma alteração antiga fora da janela. O resultado continua sendo incremental com cobertura recente.

### 4.4. Conferência limitada e mudanças concorrentes

Eliminar a segunda passagem integral do modo incremental.

Se houve novidades ou coleta de mais de duas páginas, permitir no máximo **uma nova leitura da primeira página** para verificar o cabeçalho da coleção. Essa leitura consome o orçamento de requisições. Uma coleção obtida integralmente em uma única página não exige conferência adicional por padrão.

Se o cabeçalho mudar durante a coleta:

- Preservar metadados já importados.
- Não publicar o cursor da coleção nem descartar a referência congelada.
- Pausar a coleta como janela instável, com estados candidatos ainda pendentes.
- Na retomada, revalidar a janela recente dentro do mesmo orçamento configurado.
- Não reiniciar outra coleção já concluída e não executar reconciliação completa automaticamente.

Na validação incremental, variação no `total` não deve, isoladamente, acionar o erro global `Unstable` da implementação atual. O total é um sinal de possível mudança, não um snapshot.

Comparar identidade e data antes da validação temporal. Um ID confirmado na referência congelada com `added_at` diferente, inclusive quando a data anterior é nula, representa uma atualização do mesmo item. Registrar a nova data candidata, reiniciar a contagem de páginas conhecidas e continuar a busca. Essa data mutável fica fora da comparação temporal entre vizinhos e não substitui `previous_added_at` no checkpoint. A publicação continua atômica por coleção, incluindo após interrupção e retomada. Registrar a política como `descending_except_date_updates` em `ordering_observed`.

Na retomada, reconstruir a última referência temporal pela menor data das observações já gravadas que não sejam atualizações de data de IDs confirmados. Essas observações seguem a ordem decrescente verificada pela coleta. Isso evita herdar de versões anteriores um `previous_added_at` baseado em data mutável, sem refazer chamadas de dados nem descartar o progresso.

Para músicas curtidas, se a ordem dos demais itens observados deixar de ser compatível com adições recentes no início, ou os dados não permitirem avaliar as fronteiras, publicar apenas metadados de catálogo e pausar a descoberta com cobertura inconclusiva. Informar a necessidade de revisão ou reconciliação explícita. Não certificar completude nem consumir a biblioteca inteira como fallback. IDs repetidos e mudanças nas assinaturas das páginas continuam invalidando a janela, mesmo quando a data de um ID foi atualizada.

## 5. Publicação dos estados

Separar publicação incremental da reconciliação completa.

No incremental, publicar por coleção somente depois de alcançar uma condição válida de término. Na mesma transação:

1. Aplicar `is_saved=true` e `saved_at` para os itens positivamente observados.
2. Registrar a confirmação desses itens pelo job.
3. Avançar o estado e a referência da coleção para a próxima execução.
4. Registrar cobertura `recent_window` e o motivo da parada.

**É proibido executar a lógica `UPDATE ... SET is_saved=0 WHERE id NOT IN (...)` em um job incremental.** A função atual `database::finish` não pode ser reutilizada sem separar essa responsabilidade.

Falha, quota, orçamento esgotado ou janela instável não avançam o cursor da coleção nem publicam seus estados pendentes. O catálogo importado pode permanecer. Uma coleção já publicada não é revertida porque a coleta de outra coleção ou a fila de faixas falhou depois.

## 6. Álbuns e metadados

A coleta de álbuns usa a mesma heurística das curtidas, com estado e checkpoints próprios.

No modo econômico, importar faixas complementares somente de:

- Álbuns recém-salvos cuja importação de faixas esteja ausente ou incompleta.
- Álbuns observados na janela recente cuja quantidade de faixas mudou.
- Itens pendentes de uma importação anterior.

Reutilizar as faixas embutidas na resposta de `/me/albums`. Consultar somente as páginas adicionais necessárias e deduplicar a fila.

Não percorrer todos os álbuns locais para buscar TTLs vencidos em uma sincronização normal. A atualização ampla de metadados e de faixas fica no `--full`. A reconciliação dos estados, em `--reconcile`, não força essa atualização ampla.

Um álbum removido e salvo novamente não exige baixar suas faixas outra vez se a importação existente continuar completa e compatível com a quantidade observada.

## 7. Remoções e estado conhecido

A busca de novidades não encontra necessariamente uma música descurtida há anos no meio da coleção. Até uma verificação direcionada ou reconciliação, `is_saved` representa a última confirmação conhecida.

Exibir em `status`:

- Data da última busca incremental por coleção.
- Data da última reconciliação completa.
- Cobertura da última execução.
- Se remoções foram ou não verificadas.

Consultas existentes mantêm seus filtros, mas devem indicar em stderr quando os estados dependem de uma reconciliação antiga ou de cobertura recente, preservando o JSON em stdout. Um modo explícito de saída com metadados pode ser acrescentado sem quebrar o formato antigo.

Para poucos itens selecionados, `check-saved` deve usar uma consulta em lote de pertencimento à biblioteca. A referência atual de `GET /me/library/contains` aceita até **40 URIs** por requisição e retorna booleanos. Usar URI Spotify estável, respeitando a normalização e o tratamento de relinking.

- Atualizar somente os itens explicitamente confirmados pela resposta. Para gravar no banco, exigir que o item já exista no catálogo; uma URI desconhecida pode ter seu resultado exibido, mas não dispara enriquecimento automático nem cria metadados inventados.
- Validar a quantidade e o mapeamento das respostas antes de publicar o lote.
- Um booleano falso confirmado pode atualizar `is_saved=false` para aquele item, preservando histórico e metadados.
- Erro HTTP, resposta incompleta ou endpoint indisponível não significa item removido.
- Não consultar todos os IDs locais automaticamente; verificar 21.948 itens em lotes de 40 ainda custaria 549 requisições.
- Não acrescentar escopos de playlists ou seguidores para trabalhar apenas com músicas e álbuns.

Fonte: [Check User’s Saved Items](https://developer.spotify.com/documentation/web-api/reference/check-library-contains).

## 8. Orçamentos e proteção contra varredura acidental

Valores iniciais propostos, configuráveis:

| Parâmetro | Padrão | Regra |
|---|---:|---|
| Tamanho da página de biblioteca | 50 | Máximo dos endpoints consultados. |
| Páginas consecutivas conhecidas para parar | 2 | Não contar a mesma página duas vezes. |
| Orçamento de requisições por coleção e execução | 20 | Inclui conferência e tentativas repetidas; renovação OAuth é contabilizada separadamente. |
| Orçamento de requisições de faixas complementares por execução | 20 | Consome a fila gradualmente. |
| Conferências adicionais do cabeçalho | Até 1 por coleção | Fazem parte do orçamento. |
| Páginas de sobreposição na retomada | 2 | Ajustar ao início da coleção quando necessário. |

São limites por invocação da CLI, não um limite permanente de profundidade. Cada nova tentativa HTTP consome orçamento. Reservar espaço para uma conferência final quando ela for necessária.

Ao atingir o orçamento:

- Persistir o lote e o checkpoint.
- Marcar `paused` com motivo `request_budget_exhausted`.
- Não tratar a descoberta incompleta como sucesso nem avançar sua fronteira.
- Mostrar quantas chamadas foram usadas e como continuar com `--resume` ou aumentar o limite explicitamente.
- Nunca mudar de modo para uma varredura completa.

O limite do Spotify continua independente desses orçamentos. Preservar pacing, backoff, `Retry-After`, cooldown persistente e bloqueio imediato em `QUOTA_EXCEEDED`.

## 9. Retomada sem perder o ganho incremental

Persistir a referência congelada, o cabeçalho observado, o próximo offset, fingerprints das páginas de sobreposição e os estados candidatos.

Na retomada:

1. Respeitar o cooldown antes de qualquer chamada de dados.
2. Revalidar o cabeçalho e as páginas próximas ao checkpoint.
3. Se as âncoras coincidirem, continuar do próximo trecho pendente, com sobreposição limitada.
4. Se divergirem, reposicionar a coleta a partir do início da janela recente, mantendo o catálogo e a mesma referência anterior à coleta.
5. Aplicar o orçamento da nova invocação e informar a cobertura limitada.

Essa validação por âncoras é uma otimização heurística. Não é prova de que toda a paginação intermediária permaneceu idêntica.

Itens gravados pelo job interrompido não podem passar a ser considerados conhecidos apenas por já estarem no SQLite. Isso faria uma retomada parar cedo e perder novidades ainda não alcançadas.

O orçamento não pode criar um ciclo em que cada `--resume` baixa sempre as mesmas primeiras 20 páginas de uma biblioteca estável. A retomada deve avançar quando suas âncoras forem válidas.

## 10. Persistência proposta

Criar migrations adicionais, sem recriar o banco.

### `library_sync_state`

Chave composta por conta e coleção, contendo:

- `initial_import_completed_at`;
- `last_incremental_completed_at`;
- `last_full_reconciliation_at`;
- `watermark_added_at`, apenas auxiliar;
- `remote_total_last_seen`;
- `head_signature` e informação sobre a ordem observada;
- `last_coverage` e `last_successful_job_id`.

### `sync_baseline_item`

Referência congelada por `job_id`, coleção e ID, com `saved_at`. Não armazenar JSON completo. Manter até a conclusão ou descarte seguro da coleta correspondente.

### Extensões do job e das observações

Persistir:

- `sync_mode`: `initial`, `incremental`, `reconcile`, `full`;
- `coverage`: `recent_window`, `full_listing`, `inconclusive`;
- motivo da pausa e da parada;
- orçamento configurado e requisições consumidas por escopo;
- `consecutive_known_pages` e progresso da janela;
- estado independente de publicação de curtidas, álbuns e fila;
- estado salvo anterior e timestamp candidato quando necessário.

Separar “coleção recente processada” de “biblioteca inteira reconciliada”. Um incremental nunca atualiza `last_full_reconciliation_at`.

## 11. Compatibilidade e pontos do código

- `src/main.rs`: selecionar o novo modo padrão, adicionar comandos e limites, manter consultas offline e formato de saída compatível.
- `src/sync.rs`: separar o coletor incremental do coletor completo; remover a segunda passagem integral somente do caminho incremental.
- `src/database.rs`: separar publicação positiva, confirmação direcionada de remoções e reconciliação por ausência; transações por escopo.
- `src/spotify.rs`: permitir parâmetros tipados além de `offset`, necessários para consultas com `uris`; manter origem fixa e proteção de credenciais.
- `migrations/`: acrescentar estado e referências persistentes.
- README e ajuda: explicar os modos, os orçamentos e a cobertura.

Para bancos da v0.1.0:

1. Preservar contas, credenciais, configuração do callback, catálogo, estados e fila.
2. Reconhecer importações completas já concluídas sem exigir outra varredura.
3. Inicializar o estado a partir de jobs concluídos e das observações locais disponíveis; um `MAX(saved_at)` isolado não prova importação inicial concluída.
4. Manter `sync --resume` compatível com um job antigo em andamento. Ele pode concluir usando o protocolo anterior; não descartar o trabalho importado silenciosamente.
5. Se um estado não puder ser reconstruído, explicar o motivo. Não iniciar uma coleta completa cara sem comando explícito quando já houver evidência de importação concluída.

## 12. Critérios de aceite e testes obrigatórios

- [ ] Banco novo faz a importação inicial; banco já completo entra no modo econômico.
- [ ] Base com 21.948 curtidas, sem mudanças e com coleções maiores que duas páginas: normalmente 2 chamadas de curtidas + 2 de álbuns + 1 de perfil, sem retries ou faixas pendentes. Não reler centenas de páginas.
- [ ] Cinco curtidas novas no início: percorrer a página com novidades e duas páginas conhecidas, com no máximo uma conferência do cabeçalho.
- [ ] Item cadastrado no catálogo, mas não curtido, não provoca parada prematura.
- [ ] Descurtir e curtir novamente o mesmo ID atualiza sua data e é tratado como observação nova.
- [ ] IDs com timestamps iguais são processados sem usar a data como corte exclusivo.
- [ ] Total igual com adições e remoções compensadas não causa atalho incorreto.
- [ ] Remoção fora da janela não é inventada nem anunciada como detectada; estado informa a cobertura.
- [ ] `--reconcile` detecta remoções após sua validação completa e conserva o catálogo.
- [ ] `check-saved` altera somente os itens explicitamente confirmados, em lotes de até 40 URIs.
- [ ] Incremental não executa SQL de remoção por ausência nem a segunda passagem integral.
- [ ] Falha ou interrupção mantém lote e checkpoint atômicos e não avança a fronteira incompleta.
- [ ] Retomada usa a referência congelada, e não os próprios itens recém-importados como fronteira.
- [ ] Várias retomadas de uma janela estável avançam além do orçamento de uma única execução.
- [ ] Cabeçalho ou sobreposição alterados não disparam varredura completa automática.
- [ ] Ordem incompatível gera cobertura inconclusiva, sem promessa de completude.
- [ ] Cache vencido de milhares de álbuns não produz atualização ampla em `sync`.
- [ ] Uma coleção concluída não é refeita por falha posterior em outra coleção ou nas faixas.
- [ ] Orçamento, quota e tentativas são contabilizados e persistidos sem repetição agressiva.
- [ ] Upgrade de uma base completa da v0.1.0 não exige apagar dados, repetir login ou importar tudo novamente.
- [ ] Job legado interrompido continua retomável, com seu modo anterior explícito.

## 13. Referência de consumo

| Cenário, sem retries | Referência proposta |
|---|---|
| Nenhuma novidade, duas coleções grandes | 5 chamadas de dados: perfil + 2 páginas por coleção. |
| Cinco curtidas novas, álbuns sem mudança | Até 7 chamadas: perfil + 3 páginas de curtidas + 1 conferência + 2 páginas de álbuns. |
| Álbum recém-salvo com faixas incompletas na resposta | Chamadas da janela recente + somente as páginas adicionais desse álbum, dentro do orçamento. |
| Muitas novidades além do orçamento | Pausa com checkpoint; retomadas continuam o trabalho. |
| Reconciliação completa solicitada | Custo proporcional ao tamanho da biblioteca; separado do uso diário. |

Essas contagens são metas verificáveis em testes com dados estáveis, não uma garantia de tempo ou quota do Spotify.

## 14. Entrega da implementação futura

Entregar migrations compatíveis, CLI atualizada, testes, documentação e build Windows x64. Demonstrar em log o modo escolhido, a cobertura, o motivo da parada e a quantidade real de requisições. Não considerar concluído se o comando normal ainda percorrer a biblioteca inteira após uma importação inicial válida.

### Atualização: ordenação dos álbuns salvos

A coleta de álbuns não exige ordem decrescente de `added_at`, mesmo para IDs ausentes da referência. Esses IDs são novidades e reiniciam a contagem de páginas sem alterações. Permanecem a comparação de IDs/datas, a rejeição de IDs repetidos e as conferências de assinaturas, limites e retomada. Usar `page_signatures_and_known_ids` em `ordering_observed` para álbuns; a política temporal das músicas permanece separada. A cobertura de janela recente continua heurística e não certifica alterações fora das páginas consultadas.
