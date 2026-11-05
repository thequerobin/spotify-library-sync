# Alterações

## Próxima versão

- Pausas por ordem incompatível informam coleção, página, ID, datas UTC e classificação na referência, sem iniciar uma conferência completa.
- Incremental reconhece mudanças de data de IDs já confirmados antes de validar a ordem, atualiza a data sem duplicar itens e continua até páginas sem alterações.
- Checkpoints conservam a referência temporal dos demais itens; retomada, conferência de páginas e publicação atômica permanecem ativas para músicas e álbuns.
- Avisos de rate limit no fuso local do sistema, com data, hora, deslocamento UTC e tempo restante atualizado.
- Apresentação compartilhada entre menu/CMD, comandos, HTTP 429 e QUOTA_EXCEEDED; cooldown e status persistidos em UTC.
- Testes de fusos, virada de data, vencimento, arredondamento, bloqueio persistido e seleção local durante cooldown.

## 0.4.0 — 05/11/2026

- Menu interativo no CMD ao executar sem argumentos; o iniciador Windows abre o menu.
- Playlists LSUA e USSA, com nomenclatura do Spotifast e seleção exclusivamente no SQLite.
- Períodos por adição ou lançamento, prévia, avisos de cobertura, nome e visibilidade editáveis.
- Autorização opcional `login --playlists`, preservando o login de leitura existente.
- Seleção congelada e envio em lotes persistentes; recuperação de criação/envio sem confirmação.
- POSTs incertos não são repetidos automaticamente; retomada confere a playlist de destino sem buscar a biblioteca.
- Comandos de prévia offline, criação, status e retomada; confirmação explícita para scripts.
- 21 testes adicionais de seleção, filtros, menu, permissões, HTTP, orçamento e retomada.

## 0.3.0 — 05/11/2026

- Discografias exclusivamente dos artistas seguidos, com autorização opcional `login --discography`.
- Filas persistentes de artistas e faixas, retomada por página e orçamento global de tentativas HTTP.
- Cache por conta, mercado e filtro; álbuns compartilhados entre artistas sem importação repetida.
- Primeira listagem completa, TTL de sete dias e atualizações recentes com referência congelada.
- Lotes de 100 artistas por padrão, priorizando artistas nunca importados, e modo de descoberta sem faixas.
- Consultas offline de seguidos e lançamentos, status de cobertura e fila separada da biblioteca.
- Preservação de curtidas, álbuns salvos e catálogo de artistas não mais seguidos.
- 22 testes adicionais, incluindo 1.000 artistas, mudanças concorrentes, orçamento, permissões e transporte HTTP.

## 0.2.1 — 05/11/2026

- Importação completa retoma o offset pendente buscando referências por ID/data nas páginas próximas.
- Primeira passagem e conferência possuem checkpoints independentes.
- Mudanças detectadas na conferência preservam a importação e repetem somente a confirmação.
- Busca de referência limitada e configurável com `--resume-search-pages`.
- Compatibilidade com jobs antigos sem referências ordenadas; status exibe os dois offsets.
- Sete testes novos de retomada, offsets deslocados, limites, referências removidas e conferência interrompida.

## 0.2.0 — 05/11/2026

- `sync` passa a buscar novidades nas páginas recentes depois da primeira importação completa.
- Referência local congelada, parada após duas páginas conhecidas e conferência limitada do cabeçalho.
- Orçamentos persistidos, incluindo retries, e retomada com validação de âncoras.
- Publicação independente de curtidas e álbuns, sem remover itens ausentes de uma janela parcial.
- `--reconcile` confirma remoções; `--full` preserva a atualização ampla explícita.
- `check-saved` confirma seleções em lotes de até 40 URIs.
- Migração compatível com v0.1.0 e retomada de jobs legados.
- Cache de álbuns completos reutilizado no incremental, sem atualização ampla por TTL.
- Status de cobertura, pausas e fila; cooldown persistente também para consultas direcionadas.
- CI com testes e pacotes para Linux, Windows e macOS.
